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

`android_res_cancel` closes this process's descriptor; it does not cancel or join
Android's resolver work. Android releases its own query-limiter slot when that
work returns, on a lifetime the daemon cannot observe.

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

On the app-UID path, the resolver transaction is independent of its TCP
transport. Closing the transport discards a late answer rather than cancelling
Android's query or delivering it to a reused flow. Clean transport completion may
retain client-facing TCP state as described in
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

Failures in the daemon's wrapper -- making the returned descriptor nonblocking,
registering it with Tokio, or polling that registration -- are local failures.
On the app-UID path they end the session because subsequent queries depend on the
same facility. Once ingress observes one, it commits no further query. The
descriptor, buffers and local reservation are released during joined teardown;
the Android operation finishes independently.

The app-UID daemon reserves at most 32 logical resolver tokens, below Android's
256-query per-UID limit. A UDP query owns one token. DNS-over-TCP owns one per
transport and transfers it to an outstanding query if the transport closes; it
does not charge a second token per query.

Root listener setup failure does not stop the session. Routing omits the missing
redirect so ordinary traffic and manually configured downstream DNS may continue.
Transient active-listener accept failures are retried; cancellation-time failures
are teardown.

See [`errors.md`](errors.md) for single-delivery report routing.
