# vpnhotspotd Internals

`vpnhotspotd` is the native daemon used by VPNHotspot for all long-running
background root-side work, including but not limited to routing state, DNS
proxying, neighbour monitoring, and IPv6 NAT mode. Running a JVM process as
root is much more expensive, so that work lives here instead.

The same binary has a second, app-UID entry point for Shizuku mode. It owns no
root state: it receives a duplicate of a TUN the Kotlin side published and owns
the dataplane over it, described in [`shizuku.md`](shizuku.md). The two entry
points share control framing, report delivery, low-level socket helpers and the
platform-neutral library. Their runtime state, owned resources and system
mutations are independent.

These docs describe the daemon's internal ownership model and cleanup
invariants. For the IPC schema, see
[`mobile/src/main/proto/daemon.proto`](../../mobile/src/main/proto/daemon.proto).

## Source Map

This is a locator. The contracts live in the subject documents linked from
[Documents](#documents) below.

[`src/root/`](../../mobile/src/main/rust/vpnhotspotd/src/root/) contains the root
entry point and everything it owns: control and session state, routing, NAT66,
DNS, traffic counters, netlink, neighbour monitoring and the IPsec probe.

[`src/shizuku/`](../../mobile/src/main/rust/vpnhotspotd/src/shizuku/) contains the
app-UID entry point and dataplane: the control conversation, the TUN handoff and
config calls, TUN ingress and egress, TCP, UDP, Echo and virtual DNS.

The small set of cross-mode Android runtime modules stays directly under
[`src/`](../../mobile/src/main/rust/vpnhotspotd/src/):
[`control_wire.rs`](../../mobile/src/main/rust/vpnhotspotd/src/control_wire.rs)
frames the two control conversations,
[`report.rs`](../../mobile/src/main/rust/vpnhotspotd/src/report.rs) delivers
structured reports,
[`socket.rs`](../../mobile/src/main/rust/vpnhotspotd/src/socket.rs) contains
shared nonblocking socket operations, and
[`android_network.rs`](../../mobile/src/main/rust/vpnhotspotd/src/android_network.rs)
binds a socket to an Android `Network`.

[`src/shared/`](../../mobile/src/main/rust/vpnhotspotd/src/shared/) is the
platform-neutral library compiled by host tests. It contains protocol models,
packet parsing and construction, accounting and task-ownership logic. See
[`errors.md`](errors.md) for the reporting split.

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
