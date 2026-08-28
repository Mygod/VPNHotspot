# DNS

The DNS runtime is per session. It owns per-MAC daemon listeners for downstream
DNS and the handoff to Android's resolver API. Routing owns how downstream
packets reach those listeners.

## Listener Ownership

[`root::dns::Runtime::start`](../../mobile/src/main/rust/vpnhotspotd/src/root/dns.rs)
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

The rootless TestNetwork path does not share the code above: it has its own
resolver owner in
[`shizuku/resolver.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shizuku/resolver.rs), whose
only caller is `shizuku/virtual_dns.rs`, while root's DNS proxy keeps
[`root/dns.rs`](../../mobile/src/main/rust/vpnhotspotd/src/root/dns.rs) unchanged. It watches
both directions of the resolver descriptor and treats closure of that descriptor,
however the platform closes it, as completion of the transaction; the readiness
bits behind that are in
[`shizuku/resolver.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shizuku/resolver.rs). Missing a
closure would hold that query's descriptor and resolver slot until the session
ends, because transactions carry no timer.

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
carries on. A zero-length prefix is invalid DNS, and this implementation closes
the stream on it. The framing lives in
[`shared/dns_wire.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/dns_wire.rs).

Unexpected EOF while reading the next frame ends the connection cleanly.
Host- or network-unreachable response writes are treated as downstream
reachability churn and logged. Other I/O failures are returned to the
connection task.

On the app-UID path a transport task can complete while the client's own
connection is still open, and the flow's client-facing side then stays until that
close reaches `Closed` - unless its idle floor, a configuration retirement that
applies to it, or session shutdown reclaims it first and discards the remainder
([Shizuku Mode](shizuku.md#transport-completion-and-client-side-close)). Ending
a transport ends the transport only: its resolver transaction is owned separately,
is never cancelled with it, and keeps the platform's slot reserved until it
finishes, so a late answer is discarded rather than delivered to whatever reused
the connection. A session that has stopped serving refuses a question it has not
admitted rather than dropping it, so the stream stays framed, while accepted
queries finish normally.

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

Every outcome returned by the platform - no selected upstream network, a timeout,
`EBUSY` from its per-UID limiter, an unresolvable name, a remote failure -
returns SERVFAIL when possible and is otherwise silent, because a client chooses
how many queries it sends. Only the daemon's own wrapper around a transaction is
not client-driven, and a failure there is a structured, coalesced nonfatal naming
its step; see [`errors.md`](errors.md).

A submission therefore has three outcomes, which
[`shizuku/resolver.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shizuku/resolver.rs) keeps
typed all the way to the owner that acts on them:

- **`NeverReached`** - `android_res_nsend` refused the query, so nothing of
  Android's is held;
- **`Accepted`** - the platform has the query, and this process can wait for the
  DNS result or for the failure the platform returns instead;
- **`Unobservable`** - Android accepted the query and then this daemon's wrapper
  around the descriptor failed, or the readiness registration went away, so a
  per-UID slot is held whose end nothing here can observe.

The first two are ordinary answers, and the query's reservation is released with
them. `Unobservable` is not: the daemon's own memory and descriptor are released,
but the resolver slot stays reserved for the rest of the session, because
returning capacity for a transaction Android still holds is what drives its
limiter into `EBUSY`.
A DNS-over-TCP stream that hits this is closed, because no resolver capacity
remains for it to ask another query with. Root mode has no per-UID resolver
ceiling of its own and reserves nothing.

Listener setup failures do not stop the session.
Routing omits the missing DNS redirect, so normal IP traffic and manually
configured downstream DNS can still work.

Per-MAC listener setup and routing failures remove only that MAC/protocol DNS
capability. If a listener was staged but routing did not commit the matching
MAC redirect and direct-port guard, the staged listener is cancelled before the
session publishes committed capabilities. A TCP listener accept failure after
cancellation is treated as teardown; transient active-listener accept failures
are retried.
