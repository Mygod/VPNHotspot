# Invariants

These rules apply across daemon modules. Put subsystem-specific detail in the
owning document.

## Ownership

- A root session owns runtime state for one immutable downstream interface.
- DNS and NAT66 own listeners and proxy state; routing owns the interception
  that sends traffic to them.
- Root traffic identity is MAC-facing. IP addresses may select counters but are
  not live client identity.
- Each rtnetlink connection has one owner. Event connections are separate from
  request connections, and session routing retains its request connection.
- The process-wide NAT66 ICMP dispatcher owns NFQUEUE `30000`; client identity
  for queued Echo Requests comes only from six-byte hardware-address metadata.
- The process-wide NAT66 UDP reply-socket registry owns at most one socket per
  exact source address and port. Only the last lease may remove and close it.
- App-UID TUN ingress owns all TUN-visible transport state. The session owns and
  joins both dataplane tasks, and ends when either task completes.
- An app-UID TCP flow is identified by socket handle plus incarnation because
  smoltcp reuses handles.
- App-UID workers are never detached. Cancellation is followed by join. For an
  upstream TCP worker, its terminal proves the captured descriptor is closed and
  releases its lease immediately; the memory-only flow record may remain for the
  client closing handshake.
- Client TCP bytes cross a bounded per-flow stream. Backpressure is lossless;
  readiness comes from that stream and packet/timer events, not capacity polling
  or a second wake protocol.
- A client FIN is delivered after all preceding bytes. Clean transport completion
  closes the upstream socket and releases its lease but retains client-facing TCP
  state until its close handshake finishes, or until expiry, explicit flow
  teardown or session shutdown.
- Flow teardown and every descriptor-lease release happen exactly once, although
  an upstream lease may end before its memory-only client flow. Accepted resets
  are determined by the TCP stack, not by inspecting a header bit alone.
- Android resolver work cannot be joined. Each submitted DNS-over-TCP
  transaction, not its transport, owns its resolver descriptor and query buffer
  until settlement; transport closure does not cancel it. Resolver terminal
  settlement releases descriptor capacity before answer delivery. Daemon
  admission is independent of Android's per-UID query limit.
- DNS-over-TCP resolver waits are readiness-driven and transaction-table-owned.
  Every wait has one row while the table is live; ownership mismatches are
  reported and the affected delivery is discarded. See [`dns.md`](dns.md).
- Each conversation owns one nonfatal reporter, and a successor cannot install
  it while its predecessor is finishing.
- Nonfatals coalesce by compiled source site only while their one writer handoff
  is occupied; blocked sites drain in first-blocked order when it returns.
- Root waits for every detached report-capable task and destructor before
  finishing its reporter.
- Failed session startup cancels staged resources. Cancellation after downstream
  discovery waits for a complete `Session`, then uses its normal rollback.
- Tracked packet loops observe cancellation at blocking waits and within
  continuously ready drains. Rtnetlink request drivers end when their owning
  connection drops.

## App-UID Resource Policy

- Admission accounts descriptors only. Its ceiling is the soft
  `RLIMIT_NOFILE` less the descriptors measured open at session start. Exactly
  one unit within that ceiling is protected for DNS resolver work; it is a
  floor general traffic cannot enter, not a DNS concurrency ceiling or an
  eagerly opened descriptor.
- Every admitted unit is capacity for at most one traffic-created descriptor: a
  UDP mapping socket, TCP upstream-flow socket, opened Echo family socket or
  submitted resolver descriptor. TCP DNS may reserve its unit while receiving
  the admitted framed body, but releases it if submission never returns a
  descriptor. Denial refuses the new owner only; after a descriptor exists, its
  lease is released after descriptor close and, where a worker owns it, worker
  join. A virtual-DNS TCP transport and retained client-closing state own no
  descriptor lease.
- The daemon has no aggregate memory-share calculation, byte ledger or
  byte-precharge model. Memory-only downstream state, including UDP send
  histories, queued TUN datagrams, UDP/Echo reply events and aggregate TCP-DNS
  owner requests, grows dynamically and is dropped on session stop/restart or
  process exit. Real memory exhaustion may terminate the recoverable app-UID
  child.
- Fixed TCP buffers, protocol-size buffers, depth-one per-flow DNS control
  handoffs, the descriptor-derived virtual-UDP DNS completion handoff, protocol
  maximums, and their exact full/expiry behavior are catalogued in
  [`shizuku.md`](shizuku.md#bounded-buffers-and-handoffs) and
  [`dns.md`](dns.md). No table capacity may be inferred from traffic counter
  classes.

## Interception

- Never install interception for a runtime capability that did not start.
- DNS and NAT66 TCP/UDP listeners are per-MAC and per-protocol. NAT66 ICMP is a
  session capability. Failure of one optional capability must not disable the
  others.
- DNS and NAT66 TCP/UDP carry identity through MAC-matched interception. NAT66
  ICMP uses `NFQA_HWADDR`; source-IP neighbour lookup is not a fallback.
- Per-MAC listener ports are not client-provided identity tokens. Direct access
  that bypasses MAC interception must fail closed.
- ICMPv6 link-local control and local downstream traffic are outside upstream
  interception, blocking and accounting.

## Cleanup

- Cleanup of state that can outlive the process must not depend on private app
  storage or daemon memory.
- Session rollback may use its applied-mutation list. Clean reconstructs state
  from deterministic identifiers and current system state.
- Missing cleanup state is expected; unexpected failures are structured reports.
- Do not flush shared platform state without an app-owned identifier.
- Every persistent route, rule, address, mark, table or firewall mutation needs
  a documented Clean path.
- Reply-socket leases are process-local. Stop and replacement release their
  leases; process death closes remaining descriptors.
- Legacy mutation compatibility is normally unnecessary because reboot clears
  this state; do not add migration cleanup without a concrete need.

## Configuration

- Root packet and query work reads config snapshots without holding the mutex
  across I/O. Replacement may hold the mutex while routing commits, then publish
  the matching snapshot.
- Root clients are keyed by MAC and may have no IPv4 address. An empty client set
  defers NAT66 rather than failing it.
- `ShizukuSessionConfig` is level-triggered and contains only `admit`; the
  newest value is the whole truth. Its ACK is sent only after TUN ingress has
  applied that value.
- Changing `admit` tears down nothing. While false, ingress is dropped and
  creates or refreshes no state; existing deadlines and protocol endings
  continue.
- The app-UID session MTU is immutable and is checked once against the TUN in
  `StartShizukuSessionCommand`.

## Errors

- Terminal errors mean the requested call could not complete.
- Nonfatal reports mean the broader operation continued after losing an optional
  capability or observing unexpected background state.
- Unexpected daemon-owned networking, resolver-wrapper, routing, firewall,
  descriptor, process or cleanup failures must be structured reports, not
  stderr-only logs.
- Cancellation is not an error; report only cleanup failures or broken
  invariants exposed during cancellation.

## Platform Assumptions

- The app-UID daemon does not explicitly bind its process or egress sockets to
  an Android `Network`; Android's UID policy is the only upstream selector.
- App-default changes are not daemon state boundaries. TCP, unconnected UDP and
  Echo follow ordinary Android socket behavior rather than being reset, and
  queued events are fenced only by their owning flow or worker identity.
- App-UID DNS submits with `NETWORK_UNSPECIFIED`. Android's DNS proxy selects
  the query network from the peer UID; a submitted transaction remains owned
  until settlement even if the default network changes.

Public compatibility assumptions belong in the root [`README.md`](../../README.md).
Keep hardcoded AOSP-derived behavior beside its source and do not duplicate the
hidden-API inventory here.
