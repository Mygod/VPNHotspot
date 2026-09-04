# DNS

Root DNS is a per-session, per-MAC proxy. The app-UID path uses the same Android
resolver API for virtual DNS without physical-client attribution.

## Root Listener Ownership

[`root::dns::Runtime::start`](../../mobile/src/main/rust/vpnhotspotd/src/root/dns.rs)
stages independent TCP and UDP listeners for each allowed MAC. Routing publishes
only ports whose matching MAC redirect and direct-port guard committed. Failure
of one listener or rule is a structured nonfatal and removes only that
MAC/protocol capability.

IPv4 DNS reaches listeners through MAC-matched DNAT. `filter INPUT` guards require
the conntrack original destination to be the downstream gateway on port 53, so a
client cannot use another client's listener port. Packets still addressed to the
gateway are rejected rather than falling through to another local DNS service.

NAT66 TCP/UDP uses the same handlers from its per-MAC listener context, preserving
resolver selection, identity and accounting.

Root TCP accept backpressure is kernel-owned through the runtime
`net.core.somaxconn`; a full accept queue allocates no Rust connection state.
Listener setup failure removes only that MAC's TCP DNS capability.

Each root UDP listener reuses one full-datagram receive buffer so it can collect
a supported datagram in one read; truncated bytes cannot be recovered. A
dispatched task copies only the datagram it received. Memory exhaustion
terminates the root daemon; there is no aggregate root DNS memory quota.

## Root Config Snapshots

Each query or TCP connection clones `SessionConfig` before resolving and selects
`primary_network`, then `fallback_network`, otherwise failure. Resolver I/O never
holds the config mutex. Counters follow the proxy operation, not a physical
upstream interface.

## Resolver Handoff

Both paths use bionic:

- `android_res_nsend` submits one query with a network selection. Root passes
  the configured `Network`; the app-UID path passes `NETWORK_UNSPECIFIED`, so
  dnsproxyd chooses from the peer UID's policy for that submission;
- `android_res_nresult` reads and closes the result;
- dropping an unfinished query calls `android_res_cancel`.

When a root or app-UID resolver transaction completes, `android_res_nresult`
receives one caller-owned full-answer buffer. The answer retains that buffer
until it is sent, discarded or dropped during shutdown. Memory exhaustion
terminates the root daemon or recoverable app-UID child.

`android_res_cancel` closes only this process's descriptor; it neither cancels
nor joins Android's resolver work. The per-UID limiter runs asynchronously after
descriptor handoff, so refusal arrives as `-EBUSY` through that descriptor. The
daemon neither mirrors the platform limit nor derives local resource bounds from
it. Accepted queries still consume the app UID's Android slots;
enforcement and release remain platform-owned.

This lifecycle was verified on Android 17 and Android 13:

- Android 17: [descriptor handoff](https://android.googlesource.com/platform/system/netd/+/refs/tags/android-17.0.0_r1/client/NetdClient.cpp#519),
  [handler](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-17.0.0_r1/DnsProxyListener.cpp#1061),
  [limiter](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-17.0.0_r1/DnsProxyListener.cpp#1110),
  [cancel closes the descriptor](https://android.googlesource.com/platform/system/netd/+/refs/tags/android-17.0.0_r1/client/NetdClient.cpp#586);
- Android 13: [descriptor handoff](https://android.googlesource.com/platform/system/netd/+/refs/tags/android-13.0.0_r1/client/NetdClient.cpp#536),
  [handler](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-13.0.0_r1/DnsProxyListener.cpp#896),
  [limiter](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-13.0.0_r1/DnsProxyListener.cpp#943),
  [cancel closes the descriptor](https://android.googlesource.com/platform/system/netd/+/refs/tags/android-13.0.0_r1/client/NetdClient.cpp#603).

The descriptor is nonblocking and registered with Tokio. The daemon waits for
peer closure before calling the synchronous `android_res_nresult`, relying on the
platform assumption in the root README that `dnsproxyd` writes the complete
result before closing its one-shot socket.

The app-UID wrapper in
[`shizuku/resolver.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shizuku/resolver.rs)
watches both read and write readiness because either can carry the terminal close.
Transactions have no timer. If a platform close were never observed, the daemon's
descriptor would remain until session end; Android's own query lifetime is still
independent.

Accounting includes the DNS payload submitted and the result returned, not
physical transport, headers, retries or DNS-over-TLS. A locally generated error
does not increment resolver counters. An accepted query with no returned answer
counts only the sent side.

## TCP DNS

TCP follows the two-byte length framing in
[RFC 1035 section 4.2.2](https://www.rfc-editor.org/rfc/rfc1035.html#section-4.2.2)
and handles partial reads and writes. A resolver failure becomes a framed
SERVFAIL for that message and does not close an otherwise valid stream. A
zero-length message, malformed input from which no SERVFAIL can be built, or a
result that cannot satisfy the framing contract closes or refuses only that
stream/message.

Each framed query/response is one accounting unit. Unexpected EOF between frames
is clean. Host/network-unreachable response writes are logged as downstream
churn; other I/O failures return to the connection owner.

On the app-UID path, the transaction table—not the TCP transport—owns each
submitted query, its resolver descriptor and its ordinary userspace buffers.
Resolver terminal settlement closes the descriptor; parking or delivering the
answer retains only ordinary userspace buffers. Closing a transport does not
cancel an already submitted query, and exact flow identity prevents a late
answer reaching a reused flow. Clean transport completion may retain
client-facing TCP state as described in
[`shizuku.md`](shizuku.md#app-uid-dataplane).

## UDP DNS

Root UDP handles each datagram in its own session-owned task. A successful answer
returns to the source; host/network-unreachable sends are logged, and other send
failures are structured reports. Each query/response datagram is one accounting
unit.

## Failure Semantics

Platform outcomes such as no usable network, timeout, `EBUSY`, unresolvable
name or remote failure return SERVFAIL when possible and are otherwise silent.
They are traffic-controlled outcomes, not structured reports.

Daemon-wrapper failures are local and session-fatal on the app-UID path. Before
transaction-table insertion, unwinding closes any returned descriptor, drops the
buffers and ends its identity; afterward, the table retains ownership until
resolver settlement or shutdown. Android's operation remains independent.

There is no app-UID DNS memory budget, byte precharge or prepared-query count.
Transaction maps and wait collections grow on demand. TCP processes one framed
query at a time, so each of its two control handoffs has depth one. A refused
query-to-owner handoff ends the transport. A second owner-to-transport control is
discarded and counted unreachable; closure means that transport has already
ended. An idle TCP stream owns no resolver descriptor. All virtual-DNS TCP
transports publish their ordered requests through one unbounded session-owned
handoff to the engine. A transport remains sequential and waits for the matching
owner control before it can submit or receive another query. Cancellation is
checked before publication, a closed owner ends the transport, and the dataplane
scheduler consumes at most one aggregate request per fair pass. Session shutdown
drops requests that no longer name a live flow.

Virtual-UDP workers publish resolver terminals through an unbounded
session-owned handoff, so there is no independent query-count ceiling. The owner
reconciles each result with its worker terminal. Owner closure discards an
undelivered result; worker join and retirement still close its descriptor and
drop its buffers.

TCP completion is readiness-driven, with one transaction-table-owned wait and
one row per query. A missing wait, row or delivery identity is an owner invariant
failure: it is reported, the affected answer is discarded, and the affected TCP
session ends where it cannot safely identify the recipient.

A syntactically answerable TCP query refused before submission is drained and
receives a framed header-only SERVFAIL with `QDCOUNT=0`; the stream remains
usable. Malformed or non-query input, or a message with neither a query buffer
nor a refusal sink, closes the stream. Closing the transport does not
release an already-submitted transaction.

On stop, TCP retires its flows and directly polls each stored resolver wait once,
reporting observable wrapper failures and wait/row mismatches before dropping
pending waits. Resolver futures and descriptors are dropped with their rows;
ordinary query and answer buffers are then dropped by their owners. Virtual UDP
cancels and joins every worker. Neither path waits for Android. Process death
closes the descriptors, and app-UID DNS leaves no external state to clean up.

Root listener setup failure does not stop the session. Routing omits the missing
redirect so ordinary traffic and manually configured downstream DNS may continue.
Transient active-listener accept failures are retried; cancellation-time failures
are teardown. Root shutdown waits for detached queries, ensuring their
`android_res_cancel` runs before exit without waiting for Android itself.

See [`errors.md`](errors.md) for single-delivery report routing.
