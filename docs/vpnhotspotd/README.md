# vpnhotspotd Internals

`vpnhotspotd` is the native daemon used by VPNHotspot for all long-running
background root-side work, including but not limited to routing state, DNS
proxying, neighbour monitoring, and IPv6 NAT mode. Running a JVM process as
root is much more expensive, so that work lives here instead.

The same binary has a second, app-UID entry point for Shizuku mode. It owns no
root state: it receives a duplicate of a TUN the Kotlin side published and owns
the dataplane over it, described in [`shizuku.md`](shizuku.md). The two entry
points share the entry dispatch, the frame format and the report builders;
their runtime state, owned resources and system mutations are independent,
since no root-side mutation is permitted at the app UID.

These docs describe the daemon's internal ownership model and cleanup
invariants. For the IPC schema, see
[`mobile/src/main/proto/daemon.proto`](../../mobile/src/main/proto/daemon.proto).

## Source Map

This is a locator. The contracts live in the subject documents linked from
[Documents](#documents) below.

Root path:

- [`control.rs`](../../mobile/src/main/rust/vpnhotspotd/src/control.rs) owns the
  daemon control loop, active calls, event-style (kotlin Flow) calls that
  remain active for sessions or monitors, session slots, and the neighbour
  monitor slot.
- [`session.rs`](../../mobile/src/main/rust/vpnhotspotd/src/session.rs)
  composes one session for one downstream interface from DNS, optional NAT66,
  and routing runtimes.
- [`routing.rs`](../../mobile/src/main/rust/vpnhotspotd/src/routing.rs) and
  [`routing/`](../../mobile/src/main/rust/vpnhotspotd/src/routing/) own
  reversible route, rule, address, firewall, forwarding, and static-address
  mutations.
- [`nat66/`](../../mobile/src/main/rust/vpnhotspotd/src/nat66/) owns the IPv6
  NAT proxy runtimes and helper protocol state.
- [`dns.rs`](../../mobile/src/main/rust/vpnhotspotd/src/dns.rs) owns the daemon
  DNS listeners and Android resolver handoff.
- [`traffic.rs`](../../mobile/src/main/rust/vpnhotspotd/src/traffic.rs) owns
  daemon traffic-counter reads and the daemon-to-Kotlin counter reporting
  boundary.
- [`netlink.rs`](../../mobile/src/main/rust/vpnhotspotd/src/netlink.rs) owns the
  owner-scoped rtnetlink request and multicast-only event connections.
- [`neighbour.rs`](../../mobile/src/main/rust/vpnhotspotd/src/neighbour.rs)
  owns neighbour-monitor connections and converts netlink neighbour and bridge
  topology state into daemon events.
- [`ipsec.rs`](../../mobile/src/main/rust/vpnhotspotd/src/ipsec.rs) owns the
  optional Android 12+ IPsec forwarding-policy probe and emits session events
  for the Kotlin routing owner to perform the hidden Netd write only when
  needed.

App-UID Shizuku path, under
[`src/`](../../mobile/src/main/rust/vpnhotspotd/src/):

- `bootstrap.rs` owns the handshake and verifies the received TUN descriptor.
- `app_session.rs` owns the session loop: it applies each configuration, retires
  what the change invalidates, acknowledges, and owns the control writer.
- `tun_reader.rs` owns the TUN descriptor, the admission gate and every
  TUN-visible flow, mapping and transport, plus the tasks they start.
- `dispatch.rs` routes one read to the transport that owns it and counts what it
  cannot place.
- `tun_writer.rs`, `output.rs`, `shared/packet_writer.rs` and
  `shared/ipv4_identification.rs` are the single TUN egress path: the bounded
  queue and retirement gate, the size and fragmentation decisions, and the
  guarded IPv4 Identification allocator.
- `egress.rs` owns the selected-network sockets - the bind, hop limits, DF modes,
  error queue and received metadata - and the required raw `libc` calls.
- `udp.rs`, `reply.rs` and `send_failure.rs` own the UDP relay's mappings, the
  reply reader shared with Echo, and the errno-to-meaning mapping both use.
- `echo.rs`, `echo_session.rs` and `echo_socket.rs` own relayed ICMP Echo: the
  sessions keyed by remote and substituted sequence, and the ping sockets.
- `tcp.rs`, `tcp_device.rs`, `tcp_flow.rs`, `tcp/terminal.rs` and
  `tcp/lifetime.rs` own terminated TCP: the flow table and `smoltcp` stack, the
  TUN adapter, one flow's upstream splice, how a flow ends, and the outer idle
  floors.
- `virtual_dns.rs`, `tcp_dns.rs`, `tcp_dns/transactions.rs`, `tcp/dns.rs` and
  `resolver.rs` own the virtual DNS endpoints, the DNS-over-TCP transports and
  their transaction table, and one platform resolver transaction.
- `budget.rs`, `workers.rs` and `shared/preempt.rs` own admission accounting, the
  boundary that joins a worker before its budget reservation is released, and the
  waits retirement can interrupt.
- `gateway.rs` holds the interface's own addresses and picks the source address
  for an ICMP error the daemon originates.
- `shared/reporter.rs` and `shared/reporter/coalescer.rs` own an app-UID
  session's nonfatal coalescing, its single registration, and its finalizer.
- Platform-neutral and unit tested: `shared/classify.rs` (principals),
  `shared/extension.rs` (IPv6 extension chains), `shared/reassembly.rs`
  (ingress reassembly), `shared/tcp_wire.rs` and `shared/udp_wire.rs` (segment
  and datagram parsing), `shared/echo_wire.rs` (Echo), `shared/icmp_error.rs`
  and `shared/icmp_translate.rs` (originated and repeated ICMP errors),
  `shared/send_history.rs` (what a mapping recently sent),
  `shared/dns_wire.rs` and `shared/dns_debt.rs` (DNS framing and which
  reservation covers what), `shared/failure.rs` (local setup versus an answer),
  and
  `shared/tasks.rs` (task ownership both paths share).

Shared by both paths:

- [`report.rs`](../../mobile/src/main/rust/vpnhotspotd/src/report.rs) and
  [`shared/protocol.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/protocol.rs)
  build structured daemon reports. Delivery is not shared: root uses one
  coalescing task behind a process-global channel keyed with
  [`shared/nonfatal.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/nonfatal.rs),
  an app-UID session uses the reporter above, and dispatch is registry-first
  with no fall-through. See [`errors.md`](errors.md).

The Kotlin side has two halves. Root control lives under
[`mobile/src/main/java/be/mygod/vpnhotspot/root/daemon/`](../../mobile/src/main/java/be/mygod/vpnhotspot/root/daemon/):
it starts the binary, frames the call/reply conversation, and owns call IDs. The
app-UID path is launched and framed by
[`shizuku/AppUidDaemon.kt`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/AppUidDaemon.kt)
instead and has no call IDs. Both turn daemon reports into app-visible
exceptions or nonfatal warnings.

## Documents

- [`lifecycle.md`](lifecycle.md): daemon startup, control-loop ownership, call
  lifetime, session lifetime, cancellation, and shutdown.
- [`shizuku.md`](shizuku.md): the rootless Shizuku mode - session ownership,
  restricted TestNetwork publication, system-tethering selection, the app-UID
  dataplane, external state and cleanup, and qualification status. Its security
  posture is best effort by design: qualification showed that any app with
  network access can inject packets into the tunnel interface, so the mode
  protects tethered clients but not against other apps on the device.
- [`routing.md`](routing.md): desired routing state, reversible mutations,
  Clean behavior, and system mutation ownership.
- [`nat66.md`](nat66.md): IPv6 NAT runtime boundaries for TCP, UDP, ICMPv6,
  router advertisements, marks, and cleanup.
- [`dns.md`](dns.md): DNS listener ownership, resolver handoff, config snapshot
  semantics, and nonblocking assumptions.
- [`traffic.md`](traffic.md): MAC-facing traffic accounting, blocking scope,
  counter sources, recorder chain boundaries, and persistence mapping.
- [`errors.md`](errors.md): terminal errors, nonfatal reports, context/detail
  requirements, and background-task failure policy.
- [`invariants.md`](invariants.md): cross-module ownership, interception,
  cleanup, configuration, error, and platform-assumption rules.

## Maintenance Rule

Keep these docs in sync with daemon behavior. A change to
`mobile/src/main/rust/vpnhotspotd`, `mobile/src/main/proto/daemon.proto`, or
the Kotlin daemon controller should update `docs/vpnhotspotd` when it changes
internal ownership, lifecycle, cleanup, NAT66, DNS, routing, neighbour
monitoring, or error-reporting semantics.

Do not summarize away external side effects. If a change adds, removes, or
changes any mutation to kernel, netfilter, netd, resolver, socket, file
descriptor, process, or Android system state, the relevant doc must name:

- when the mutation happens;
- the exact external state or command shape;
- what owns rollback or normal stop cleanup;
- what Clean or process-death cleanup does, if the state can outlive the
  runtime;
- which missing-state or failure cases are expected.

For routing changes, [`routing.md`](routing.md) is a mutation catalog. Every
route, policy rule, address, iptables/ip6tables rule or chain, `ndc` request,
and Clean mutation must be listed there.

If a daemon-adjacent change does not affect those documented contracts, say so
in the change description or final response. Do not duplicate the protobuf
schema here; update `daemon.proto` comments if the wire-level contract itself
needs documentation.

Platform and compatibility assumptions that affect the public app contract stay
in the root [`README.md`](../../README.md), especially the `Other` and
`System/root command assumptions` sections. These daemon docs may link to those
assumptions, but should not create a second compatibility index.
