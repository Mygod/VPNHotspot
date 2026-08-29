# vpnhotspotd Internals

`vpnhotspotd` has two entry points:

- root mode owns long-running routing, DNS, neighbour-monitoring and NAT66 work;
- app-UID mode owns the Shizuku TestNetwork dataplane and no root state.

They share framing, reports and low-level socket code, but not runtime state or
system mutations. See
[`daemon.proto`](../../mobile/src/main/proto/daemon.proto) for the wire schema.

## Source Map

- [`src/root/`](../../mobile/src/main/rust/vpnhotspotd/src/root/) contains root
  control, sessions, routing, NAT66, DNS, traffic, netlink, neighbours and IPsec.
- [`src/shizuku/`](../../mobile/src/main/rust/vpnhotspotd/src/shizuku/) contains
  app-UID control, TUN I/O, TCP, UDP, Echo and virtual DNS.
- [`src/shared/`](../../mobile/src/main/rust/vpnhotspotd/src/shared/) contains
  platform-neutral protocol and dataplane logic covered by host tests.
- [`control_wire.rs`](../../mobile/src/main/rust/vpnhotspotd/src/control_wire.rs),
  [`report.rs`](../../mobile/src/main/rust/vpnhotspotd/src/report.rs),
  [`socket.rs`](../../mobile/src/main/rust/vpnhotspotd/src/socket.rs), and
  [`android_network.rs`](../../mobile/src/main/rust/vpnhotspotd/src/android_network.rs)
  are shared Android runtime support.

The binary modules link Android-only libraries and are not host-executed. Host
tests cover extracted shared decisions; Android builds compile-check their owner
adapters. Do not describe the adapters as host-tested.

Kotlin root control lives under
[`root/daemon/`](../../mobile/src/main/java/be/mygod/vpnhotspot/root/daemon/);
Shizuku control lives under
[`shizuku/`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/).

## Documents

- [`lifecycle.md`](lifecycle.md): process, call, session and shutdown ownership.
- [`shizuku.md`](shizuku.md): rootless mode, dataplane, system integration,
  cleanup, security limits and qualification.
- [`routing.md`](routing.md): exact route, rule, address, firewall, `ndc` and
  Clean mutations.
- [`nat66.md`](nat66.md): IPv6 NAT runtime and cleanup.
- [`dns.md`](dns.md): resolver handoff and DNS failure semantics.
- [`traffic.md`](traffic.md): accounting and blocking scope.
- [`errors.md`](errors.md): terminal and nonfatal reports.
- [`invariants.md`](invariants.md): cross-module rules.

## Maintenance Rule

Update these docs when daemon, protobuf or Kotlin controller changes affect
ownership, lifecycle, cleanup, NAT66, DNS, routing, neighbours or errors.

Document each external mutation's trigger, exact state, rollback/stop behavior,
process-death or Clean behavior, and expected missing-state failures. Every route,
policy rule, address, firewall rule or chain, `ndc` request and Clean mutation
belongs in [`routing.md`](routing.md). Public platform and compatibility hazards
belong in the root [`README.md`](../../README.md), not a second inventory here.
