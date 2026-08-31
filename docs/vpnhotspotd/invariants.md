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
  are determined by the TCP stack, not by inspecting a header bit alone. A flow
  whose own reset the interface handoff had no room for is retained rather than
  reclaimed, because its socket is the only thing that can still send it.
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
  one unit is a DNS floor that general traffic cannot consume; it is neither an
  open descriptor nor a DNS concurrency ceiling.
- Each admitted unit covers at most one traffic-created descriptor. Denial
  affects only the new owner; release follows descriptor close and worker join
  where applicable. Memory-only TCP state owns no descriptor lease.
- There is no aggregate memory budget or byte ledger. Downstream-created tables
  grow dynamically and disappear with the session or process; allocator
  exhaustion may terminate the recoverable app-UID child.
- Internet-controlled buffering is bounded instead: UDP and Echo each have a
  one-event reply mailbox, and TUN output has one queued logical datagram. Reply
  workers reserve before reading; a full TUN handoff refuses the complete
  datagram, never a fragment prefix.
- The one-ending settlement handoff uses backpressure. Its arm precedes all
  owner work except cancellation, including the wait for TUN capacity, so the
  writer cannot deadlock while returning an IPv4 Identification settlement.
- A fair pass stays frozen while output producers are gated. Deadlines continue
  retiring due state without emitting or taking a turn; capacity return resumes
  the interrupted order rather than favoring a continuously ready producer.
- TUN ingress and smoltcp polling require output capacity. The owner delivers a
  poll's packet before polling again, so unsent data and resets remain in their
  sockets during a stall. A reset candidate rechecks capacity after its required
  pre-settlement poll.
- Timed tables use exact ordered deadline indexes, and Echo errors use an exact
  family/sequence index. Point-of-use deadline checks prevent an overdue row
  from authorizing a reply before its sweep runs. smoltcp's next-poll deadline is
  recomputed only after stack state changes.
- Fragmented IPv4 output does not reuse an Identification for the same tuple
  within the documented 120-second maximum-datagram-lifetime window. Exhaustion
  and the opening quarantine affect only oversized IPv4 output.
- Output counters distinguish queue admission and refusal from actual TUN
  writes, which only the serial writer counts.
- IPv6 extension prefixes, including atomic Fragment headers, are normalized in
  one linear scan and one copy. Only genuine fragmentation enters reassembly;
  nested fragmentation proceeds one reassembly round at a time.
- Every fixed buffer, handoff, protocol maximum, timeout, and its exact
  derivation and exhaustion behavior is catalogued in
  [`shizuku.md`](shizuku.md#bounded-buffers-and-handoffs) and
  [`dns.md`](dns.md).

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
