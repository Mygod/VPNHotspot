# vpnhotspotd Internals

`vpnhotspotd` is the root-side daemon used by VPNHotspot intended for all long-running background root-side work, including but not limited to routing state, DNS proxying, neighbour monitoring, and IPv6 NAT mode.
The split is designed this way since root-side JVM daemon is much more expensive and should be avoided as much as possible.
These docs describe the daemon's internal ownership model and cleanup invariants.
IPC documentations not included and should refer to [`mobile/src/main/proto/daemon.proto`](../../mobile/src/main/proto/daemon.proto).

## Source Map

- [`control.rs`](../../mobile/src/main/rust/vpnhotspotd/src/control.rs) owns the
  daemon control loop, active calls, event-style (kotlin Flow) calls that
  remain active for sessions or monitors, session slots, neighbour monitor
  slot, and process-wide shared runtimes.
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
- [`netlink.rs`](../../mobile/src/main/rust/vpnhotspotd/src/netlink.rs) owns the
  shared rtnetlink connection, notifications, and single-consumer event slots.
- [`neighbour.rs`](../../mobile/src/main/rust/vpnhotspotd/src/neighbour.rs)
  converts netlink neighbour and bridge topology state into daemon events.
- [`ipsec.rs`](../../mobile/src/main/rust/vpnhotspotd/src/ipsec.rs) owns the
  optional Android 12+ IPsec forwarding-policy probe and emits session events
  for the Kotlin routing owner to perform the hidden Netd write only when
  needed.
- [`report.rs`](../../mobile/src/main/rust/vpnhotspotd/src/report.rs) and
  [`shared/protocol.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/protocol.rs)
  build structured daemon reports.

The Kotlin side of the same subsystem lives under
[`mobile/src/main/java/be/mygod/vpnhotspot/root/daemon/`](../../mobile/src/main/java/be/mygod/vpnhotspot/root/daemon/).
Kotlin starts the binary, frames control messages, owns call IDs, and turns
daemon reports into app-visible exceptions or nonfatal warnings.

## Documents

- [`lifecycle.md`](lifecycle.md): daemon startup, control-loop ownership, call
  lifetime, session lifetime, cancellation, and shutdown.
- [`routing.md`](routing.md): desired routing state, reversible mutations,
  Clean behavior, and system mutation ownership.
- [`nat66.md`](nat66.md): IPv6 NAT runtime boundaries for TCP, UDP, ICMPv6,
  router advertisements, marks, and cleanup.
- [`dns.md`](dns.md): DNS listener ownership, resolver handoff, config snapshot
  semantics, and nonblocking assumptions.
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
