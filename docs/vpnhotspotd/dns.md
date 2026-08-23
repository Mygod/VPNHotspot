# DNS

The DNS runtime is per session. It owns per-MAC daemon listeners for downstream
DNS and the handoff to Android's resolver API. Routing owns how downstream
packets reach those listeners.

## Listener Ownership

[`dns::Runtime::start`](../../mobile/src/main/rust/vpnhotspotd/src/dns.rs)
attempts to bind TCP and UDP listeners on ephemeral ports for each allowed
client MAC. Each MAC/protocol listener is an independent best-effort
capability. A listener that starts publishes its port to routing; a listener
that fails is reported as a structured nonfatal with downstream, MAC, and
protocol, and that MAC/protocol DNS redirect is omitted from routing.

Routing redirects downstream IPv4 DNS with MAC-matched DNAT rules to the
ephemeral ports that exist. Each redirected port also has a `filter INPUT`
guard: packets are allowed to reach the listener only when conntrack says the
original destination was the downstream gateway on port 53. Direct connections
to another client's listener port are rejected before the daemon accepts them.
Packets still addressed to the downstream gateway on port 53 are rejected in
`filter INPUT`, so blocked clients and missing capability cases do not fall
through to an accidental local DNS service.

NAT66 TCP and UDP also special-case DNS to the NAT66 gateway on port 53 and
call the same DNS handlers from the per-MAC NAT66 listener context. This keeps
resolver selection, MAC attribution, accounting, and DNS response generation in
one runtime instead of duplicating DNS behavior in NAT66.

## Config Snapshots

The DNS runtime receives the shared session config. For each query or TCP
connection, it clones a snapshot before resolving. That snapshot determines the
primary and fallback Android networks used for resolver calls.

Current selection is simple:

1. use `primary_network` when present;
2. otherwise use `fallback_network` when present;
3. otherwise return a DNS failure.

DNS does not hold the config mutex while waiting for Android resolver results.
The selected network is internal resolver state; DNS counters are not persisted
by actual upstream interface.

## Resolver Handoff

DNS queries are sent through bionic's Android resolver API:

- `android_res_nsend` starts a one-shot query on the selected Android network;
- `android_res_nresult` reads and closes the result;
- `android_res_cancel` is used if the query object is dropped before finish.

`android_res_nsend` returns a file descriptor. The daemon sets it nonblocking
and wraps it in `AsyncFd`. The daemon waits for the resolver-side socket to
become readable and then for EOF before calling `android_res_nresult`.

This shape exists because `android_res_nresult` is the public result
reader/closer. The root README records the platform assumption: the resolver
service writes the complete result before returning and closes the socket when
the result is ready, so the daemon can stay nonblocking while still using the
public result API.

### The app-UID resolver

The rootless TestNetwork path does not share the code above. It has its own resolver
owner in [`resolver.rs`](../../mobile/src/main/rust/vpnhotspotd/src/resolver.rs), whose
only caller is `virtual_dns.rs`; root's DNS proxy keeps its own resolver in
[`dns.rs`](../../mobile/src/main/rust/vpnhotspotd/src/dns.rs), unchanged.

It differs in how the descriptor is watched. Both directions are polled, and the write
direction is there purely as a close detector. `AsyncFd` offers no arbitrary-interest
poll, only one per direction, and each direction's readiness is masked to its own bits -
so an error never appears as an errno from the poll and a terminal condition written
against one would be dead code. What the two directions cover between them is every way
this descriptor can end: `EPOLLHUP` and `EPOLLIN|EPOLLRDHUP` arrive as read-closed, and a
bare `EPOLLERR` with no `HUP` beside it arrives as *write*-closed. Watching only the read
direction would instead have rested on an assumption about when the kernel raises `HUP`,
and the cost of that assumption being wrong is a transaction that never reaches a terminal
at all - holding a descriptor record, a logical token and a query until the session ends,
since there is deliberately no timer on one.

DNS accounting counts the DNS payload bytes handed to `android_res_nsend` and
the response bytes returned by `android_res_nresult`. It does not try to account
for Android's physical DNS transport, DNS-over-TLS, packet headers, or resolver
retransmits.

Locally generated DNS errors are client responses, not resolver responses. If
no resolver query is handed to Android, the daemon returns the client-visible
error without increasing DNS counters. If `android_res_nsend` accepts a query
but no resolver response is returned, only the sent query side is counted.

## TCP DNS

TCP DNS accepts normal DNS-over-TCP framing:

- read a two-byte length;
- read exactly that many query bytes;
- resolve the query through the selected Android network;
- write a two-byte response length and response bytes.

Each framed query and framed response is one DNS TCP accounting unit. DNS TCP
does not try to infer lower-layer TCP packet counts.

Nothing assumes one read is one message, and a resolver failure is not a stream
failure: a SERVFAIL is framed for the message it belongs to and the connection
carries on. Only a length prefix that can never complete - a zero-length message,
after which nothing is at an offset the framing could resynchronize on - ends it.
The framing and that decision live in
[`shared/dns_wire.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/dns_wire.rs),
away from the transport, because they are the attacker-facing half and the half
that has to be answerable for without a device.

Unexpected EOF while reading the next frame ends the connection cleanly.
Host- or network-unreachable response writes are treated as downstream
reachability churn and logged. Other I/O failures are returned to the
connection task.

On the app-UID path a transport whose client has finished asking does not end the
moment its own task returns. The task returns as soon as its ordered end of stream
has been taken, and the client's socket is still in `LAST-ACK` then - so the flow
is detached and the client's teardown finishes before anything is removed. See
[Shizuku Mode](shizuku.md#a-flow-can-outlive-its-worker); the resolver ownership
below is unchanged by it, because a detached transport asks nothing more.

A transport also ends when it falls idle past its outer TCP floor, and that ends
the *transport only*. The resolver transaction it asked for
is a row in the ingress owner's own table rather than something the transport
owns, so an expiry neither cancels, awaits nor refunds it: an outstanding
question keeps its row and its descriptor until the platform is done with it,
exactly as under an epoch retirement. The token is the transport's rather than the
question's - a live transport keeps it between questions, which is what lets the
stream ask another - so what the transport's close
does with that token is hand it to the question rather than return it, because
Android's own per-UID slot is still taken and a moment where the token looked
free would admit a query the limiter has no room for. The answer, when it comes,
is settled against the exact `(SocketHandle, worker)` pair the query was
published for, so a late answer for an expired transport is discarded and
refunded once - and the flow that reused the same handle with a new worker is
untouched by either its predecessor's answer or its predecessor's deadline.

A session that has stopped serving starts no new exchange on a transport it still
holds. A length prefix the owner has not admitted yet is refused rather than
dropped, so nothing is allocated and the stream stays framed for the client's next
question; a query the owner had already accepted, whose payload-free commit was
still queued when the stop won, is answered from the capacity that reservation
already covers rather than becoming platform work a stopping session cannot watch
the end of. Rows already submitted, and the delivery acknowledgements for them,
finish normally.

## UDP DNS

UDP DNS reads one datagram, clones the config snapshot, and resolves the query
in a child task. If resolution succeeds, the response is sent back to the
datagram source. Host- or network-unreachable reply sends are treated as
downstream reachability churn and logged with source context. Other send
failures are reported with source context.

The UDP listener intentionally does not serialize all queries through one
worker. Each query has its own child task tied to the session stop token.
Each query datagram and response datagram is one DNS UDP accounting unit.

## Failure Semantics

Resolver failure normally returns a SERVFAIL response when the query can be
parsed enough to build one. If a SERVFAIL response cannot be generated, the
query is dropped.

Every resolver outcome the platform answered - no selected upstream network, an
Android resolver timeout, `EBUSY` from its per-UID limiter, a name that does not
resolve, a remote failure - returns SERVFAIL when possible and is otherwise
silent. A client chooses how many queries it sends, so a line each would be a log
flood it drives. Only the daemon's own wrapper around a transaction is not
client-driven, and that one is a structured, coalesced nonfatal naming its step;
either way the DNS runtime keeps going.

Those two are different outcomes, not two shades of the same one, and
[`resolver.rs`](../../mobile/src/main/rust/vpnhotspotd/src/resolver.rs) keeps them
apart as types rather than distilling both into an error:

- **An expected platform outcome** - a timeout, `EBUSY`, a name that does not
  resolve, a refusal, or a submission `android_res_nsend` itself rejected - is
  what one query asked for. It becomes that query's own SERVFAIL and the stream
  carries on to the next message. What happens to the logical token depends on
  whose it is: a UDP query owns its own, so it goes back with the rest of that
  query's grant, while a DNS-over-TCP query's debt owns none in the ordinary case -
  the connection holds the one token and keeps it between questions, which is what
  lets the stream ask another.
- **An accepted-but-unobservable submission** is not. `android_res_nsend` is
  irreversible, so once it has answered with a descriptor Android holds a per-UID
  slot whatever happens here; if this process's own wrapper around that descriptor
  fails, or the readiness registration it is being watched with later goes away,
  that slot's end is something nothing here can observe. The query's record and
  bytes are refunded and its logical token is not - it is quarantined for the rest
  of the session, because refunding a slot Android still holds is what drives the
  limiter into `EBUSY`. A DNS-over-TCP stream that hits this ends rather than
  continuing: its transport no longer owns a token, so it has nothing to ask a
  next question with. Root mode has no logical-token ceiling and so has nothing to
  quarantine; it reports the step and answers the client.

Listener setup failures do not stop the session.
Routing omits the missing DNS redirect, so normal IP traffic and manually
configured downstream DNS can still work.

Per-MAC listener setup and routing failures remove only that MAC/protocol DNS
capability. If a listener was staged but routing did not commit the matching
MAC redirect and direct-port guard, the staged listener is cancelled before the
session publishes committed capabilities. A TCP listener accept failure after
cancellation is treated as teardown; transient active-listener accept failures
are retried.
