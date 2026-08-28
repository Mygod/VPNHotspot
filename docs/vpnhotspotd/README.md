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

`[[bin]]` sets `test = false`, and nothing under `src/root/` or `src/shizuku/`
is host-testable: those modules link `libandroid` and `liblog`, which exist only
in the NDK sysroot, so a host test binary containing any of them cannot be
linked. Simulating those platform calls to get around that is not allowed, and
neither is promoting the whole daemon into the library to reach them.

The consequence shapes where logic lives. The defects that cost the most were in
the *ordering between* an owner and its stack rather than in either alone, so
each such order is extracted into `src/shared/` as a platform-neutral module and
what stays beside the owner is an adapter: one method per step, each delegating
to machinery covered where it lives. `shared/ingress.rs` is the current example -
it owns the sequence one client TCP segment is handled in, and its tests drive
that sequence over two real `smoltcp` stacks with a real byte bridge between
them. What those tests do not exercise is the `tun_reader` select loop around it;
they drive the synchronous owner boundary itself, which is where every ordering
defect lived.

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
packet parsing and construction, accounting, the worker registry every dataplane
owner runs its tasks through, the round-robin order those flows take turns in and
the rule that says which flow a segment names,
the crossing between a flow's bounded byte bridge and its client-side TCP socket
and the factory that builds both sides of that bridge,
the idle floors and rearm decision that bound a terminated flow, and the
launch-policy decision the app-UID entry point applies to itself. See
[`errors.md`](errors.md) for the reporting split.

Generic owner logic lives here rather than beside its one caller when getting it
wrong would be *silent*: a worker registry that reported a terminal from a
message instead of from a join, an identity check that degraded to "this key
exists", a pass that forgot to rotate. Each of those keeps every caller compiling
and every other test green.

What does *not* move here is an owner's own wiring - its tables, its descriptors,
its spawned tasks. Those stay in the binary, and so the boundary between the two
matters: everything that is a *decision* belongs here, and what is left beside the
owner may only reach for a field. `shared/ingress.rs` is the sharpest example. It
owns the order one client TCP segment is handled in - open a flow if an unknown
`SYN` names none; settle *only* if a reset names a flow that could actually
transition because of one; push; settle; classify any accepted reset and fence
it, or arm the idle floor and extract the ending; and settle again only if that
changed something the stack has yet to act on - and every classification, counter
selection, state transition and report construction that order makes. `Engine`
supplies primitives and no decisions: a stack poll that touches no flow, a device
slot, an iterator over its table, one flow's socket and bridge reachable
together, its counters, a flow builder, and a reporter binding.

That split is not stylistic. It is what makes those decisions testable at all, and
each of them was wrong at some point: a stack poll that ran in the middle of a
client's ending, a reset acted on before the stack had judged it, an idle floor
armed after a seal had changed what the phase meant. **The host tests do not
execute `Engine`.** They execute the shared sequence over a second real `smoltcp`
stack, a real `Bridge` and a real cancellation token - so the ordering is
production's, while the flow table under it is a `Vec`.

Draw the line precisely, because it is easy to overclaim. The host tests own every
shared *decision* and are mutation-tested on the order those decisions run in.
`Engine`'s primitive bindings are a different thing: they are compile-checked and
built for Android, and they are **not** host-executed or mutation-covered. A
mutation to one of them survives every host suite. Two are worth naming. The
reporter binding reaches this process's report registries and, on their failure
path, a platform log the daemon only has on Android; and the table-scan delegation
forwards to `Engine`'s own Closed-socket walk. Each is one line that decides
nothing - what is delivered and when, and when the scan runs relative to fencing
and sealing, are shared decisions held down in `shared/ingress.rs`. What is not
covered is only whether the adapter is faithful to a primitive's contract.

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
