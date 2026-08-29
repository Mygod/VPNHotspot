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

## Root Config Snapshots

Each query or TCP connection clones `SessionConfig` before resolving and selects
`primary_network`, then `fallback_network`, otherwise failure. Resolver I/O never
holds the config mutex. Counters follow the proxy operation, not a physical
upstream interface.

## Resolver Handoff

Both paths use bionic:

- `android_res_nsend` submits one query on an Android `Network`;
- `android_res_nresult` reads and closes the result;
- dropping an unfinished query calls `android_res_cancel`.

`android_res_cancel` closes only this process's descriptor; it neither cancels
nor joins Android's resolver work. The per-UID limiter runs asynchronously after
descriptor handoff, so refusal arrives as `-EBUSY` through that descriptor. The
daemon neither mirrors the platform limit nor derives admission or storage
bounds from it. Accepted queries still consume the app UID's Android slots;
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
descriptor and local reservation would remain until session end; Android's own
query lifetime is still independent.

Accounting includes the DNS payload submitted and the result returned, not
physical transport, headers, retries or DNS-over-TLS. A locally generated error
does not increment resolver counters. An accepted query with no returned answer
counts only the sent side.

## TCP DNS

TCP follows standard two-byte-length framing and handles partial reads/writes. A
resolver failure becomes a framed SERVFAIL for that message and does not close an
otherwise valid stream. Zero-length DNS and malformed input from which no
SERVFAIL can be built close the stream.

Each framed query/response is one accounting unit. Unexpected EOF between frames
is clean. Host/network-unreachable response writes are logged as downstream
churn; other I/O failures return to the connection owner.

On the app-UID path, the transaction table—not the TCP transport—owns each
submitted query, its buffers and its charge until settlement. Closing a
transport does not cancel or recharge the query; exact flow identity prevents a
late answer reaching a reused flow. Reservations release exactly once. Clean
transport completion may retain client-facing TCP state as described in
[`shizuku.md`](shizuku.md#app-uid-dataplane).

## UDP DNS

Root UDP handles each datagram in its own session-owned task. A successful answer
returns to the source; host/network-unreachable sends are logged, and other send
failures are structured reports. Each query/response datagram is one accounting
unit.

## Failure Semantics

Platform outcomes such as no selected network, timeout, `EBUSY`, unresolvable
name or remote failure return SERVFAIL when possible and are otherwise silent.
They are traffic-controlled outcomes, not structured reports.

Daemon-wrapper failures are local and session-fatal on the app-UID path. Before
transaction-table insertion, unwinding releases the descriptor, buffers and
reservation; afterward, the table retains ownership until settlement or
shutdown. Android's operation remains independent.

App-UID DNS admission uses measured descriptor and memory budgets only. Each
submitted query precharges one DNS descriptor record plus worst-case exchange
bytes. UDP reserves a maximum question and answer; TCP reserves the announced
question, maximum answer and framed copy. TCP processes one query at a time, and
an idle stream uses no resolver capacity.

The DNS floors reserve one descriptor and enough bytes for one maximum TCP
exchange. Resolver-table capacities derive from the same record and byte bounds,
and the TCP request channel derives from the flow bound. Their storage is charged
to general headroom, so none imposes an independent query cap.

TCP completion is readiness-driven, with one transaction-table-owned wait per
query. A refused delivery split is always reported. `Split::Covered` retains the
unchanged source lease and continues delivery; `Split::Uncovered` drops buffers
before releasing their debt, then ends the TCP session or makes virtual UDP
discard only the affected answer. A completion without its row also ends the
session while preserving any wrapper failure as the terminal cause and reporting
the mismatch separately.

A syntactically answerable TCP query denied by local admission is drained and
receives a framed header-only SERVFAIL with `QDCOUNT=0`; the stream remains
usable. Malformed or non-query input, or a message with neither a query buffer
nor an admitted refusal sink, closes the stream. Closing the transport does not
release an already-submitted transaction.

On stop, TCP retires its flows and directly polls each stored resolver wait once,
reporting observable wrapper failures and wait/row mismatches before dropping
pending waits. Descriptors close before their buffers and charges are released.
Virtual UDP cancels and joins every worker before releasing its lease. Neither
path waits for Android. Process death closes the descriptors, and app-UID DNS
leaves no external state to clean up.

Root listener setup failure does not stop the session. Routing omits the missing
redirect so ordinary traffic and manually configured downstream DNS may continue.
Transient active-listener accept failures are retried; cancellation-time failures
are teardown. Root shutdown waits for detached queries, ensuring their
`android_res_cancel` runs before exit without waiting for Android itself.

See [`errors.md`](errors.md) for single-delivery report routing.
