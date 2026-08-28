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
  It is a `close()` of the descriptor this process was handed, so it recovers
  this process's descriptor and nothing of Android's work: the platform's own
  query limiter releases that operation's slot when its `resolv_res_nsend`
  returns. That external lifetime is temporary and belongs to Android; the
  daemon cannot observe, shorten or prove it, and does not model it.

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
callers are `shizuku/virtual_dns.rs` and the DNS-over-TCP transaction table in
`shizuku/tcp_dns/transactions.rs`, while root's DNS proxy keeps
[`root/dns.rs`](../../mobile/src/main/rust/vpnhotspotd/src/root/dns.rs) unchanged. It watches
both directions of the resolver descriptor and treats closure of that descriptor,
however the platform closes it, as completion of the transaction; the readiness
bits behind that are in
[`shizuku/resolver.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shizuku/resolver.rs). Missing a
closure would hold this daemon's own descriptor and the local token standing for
it until the session ends, because transactions carry no timer. It would hold
nothing of Android's: that operation ends when its own resolver work returns,
whatever this process does with the descriptor it was handed.

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
a transport ends the transport only: its resolver transaction is owned separately
and is never cancelled with it, so a late answer is discarded rather than
delivered to whatever reused the connection. Android's own operation for that
query runs to its end either way. A session that has stopped serving refuses a question it has not
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
not client-driven; see [`errors.md`](errors.md).

A submission therefore has two outcomes, and
[`shizuku/resolver.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shizuku/resolver.rs) hands
them back as an ordinary `Result`:

- **accepted** - the platform has the query, and this process waits for the DNS
  result or for the failure the platform returns instead;
- **failed** - there is nothing to wait on, and `Failure`'s own classification in
  [`shared/failure.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/failure.rs) says
  whose failure it was.

Which failure it was is the whole decision, and `Failure::ending` is the one
place it is made. `android_res_nsend` refusing - a full per-UID limiter, an
unresolvable name - is the platform's answer to one query the client chose to
send, so it is `Failure::Expected` and reaches that client as SERVFAIL, on a
DNS-over-TCP stream that carries on. The steps around it that are this daemon's
own - setting the returned descriptor nonblocking, registering it with the
runtime, and the readiness registration it is then watched with - are
`Failure::Local`. Those are not any one query's outcome: an owner whose wrapper
around the platform failed cannot wrap the next query either.

A local wrapper or readiness failure therefore **ends the app-UID dataplane
session**. The descriptor is cancelled and closed by the dropped submission, and
everything that query owed goes back: both buffers and its DNS-class descriptor
record, plus its logical resolver token for UDP, where the token is the query's
own. A DNS-over-TCP query holds no token - its transport does, for that
transport's whole life - so that one is released when the session's shutdown
closes the flow. No query state survives for it: there is no per-query quarantine
and no session-long local token reservation.

The bound the ingress owner enforces is on what happens **after** it observes
such a failure, not on when it observes one. Every path that observes one leaves
the loop with it, and the only two paths that commit a DNS submission - the
DNS-over-TCP ask arm and the per-packet dispatch - are not reached again, so no
submission is committed afterwards. What is *not* claimed is an ordering against
arrival: the ingress select is biased, but biased ordering only ranks arms within
a single poll, so a datagram can still be dispatched while a failure is in flight
from a query task that has not sent yet.

Each of the three moments such a failure can happen at reaches that bound:

- **at submission**, where the ingress owner calls the platform itself, the
  failure is returned from the submitting call and the dispatch of that packet
  stops there;
- **while a DNS-over-TCP registration is polled**, which the ingress owner also
  does itself, the settlement carrying it is taken before the next query can be
  committed;
- **while a UDP registration is polled**, which happens in that query's own task,
  the task hands the failure to the owner instead of an answer, on the channel it
  would have answered on. That handoff is not raced against the session's
  cancellation - the channel is one slot per logical token and each query sends
  at most one arrival, so it cannot block and never has to be abandoned - and
  consuming the message is what makes the failure observed exactly once. The
  query's own accounting stays on its record until session shutdown joins the
  task that sent it.

Nothing here reserves anything on Android's behalf. Root mode has no per-UID
resolver ceiling of its own and reserves nothing either; the 32-query local
ceiling is this daemon's own accounting, sized well under Android's 256-query
per-UID limit.

Listener setup failures do not stop the session.
Routing omits the missing DNS redirect, so normal IP traffic and manually
configured downstream DNS can still work.

Per-MAC listener setup and routing failures remove only that MAC/protocol DNS
capability. If a listener was staged but routing did not commit the matching
MAC redirect and direct-port guard, the staged listener is cancelled before the
session publishes committed capabilities. A TCP listener accept failure after
cancellation is treated as teardown; transient active-listener accept failures
are retried.

## Where A Resolver Failure Is Reported

Exactly one report per failure, and which owner emits it depends only on whether
that failure still has a result to travel out on.

- The **first** local failure the ingress owner observes becomes that task's
  error. It is not reported by any DNS owner: it travels to
  `shizuku/app_session.rs`, whose single routing point delivers the structured
  report the error already carries - as the start call's terminal error frame
  when that frame is still unclaimed, and as a nonfatal otherwise.
- **Additional independently observed** local failures cannot fit in that one
  result - every outstanding query fails the same way if the runtime's I/O driver
  goes - so each is routed once, locally, as a nonfatal by the DNS drain and
  shutdown path. The report emitted is the one its own failing step attached, so
  it names that step rather than the drain.
- A failure whose owner is **already gone** - the answer channel's receiver was
  dropped before the query task could hand it over - is routed once as a nonfatal
  by that task, for the same reason: there is no result left for it to travel on.
  It is never downgraded to an ordinary cancellation.

Dropping any of them would be the silent discard structured reporting exists to
prevent; emitting one that also travels would be the same failure twice.
