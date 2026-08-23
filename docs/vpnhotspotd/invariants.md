# Invariants

These are daemon-wide rules that should stay true across modules. When a change
breaks one, document the new invariant in the owning doc and explain the
compatibility or cleanup impact.

## Ownership

- A session owns daemon runtime state for exactly one downstream interface.
- The downstream interface is immutable for a session. Replacing a session may
  change config details, but not `SessionConfig.downstream`.
- DNS and NAT66 runtimes own listeners and proxy state. Routing owns system
  interception that sends traffic to those runtimes.
- Traffic accounting and client blocking are MAC-facing. IP addresses may be
  hidden counter selectors, but they are not the live client identity.
- Each rtnetlink connection has one lifecycle owner. Multicast-only event
  connections are separate from request connections; session routing retains
  its request connection for the session lifetime and never reconnects it.
- The process-wide ICMP dispatcher exists only for NAT66 ICMPv6 state that must
  be shared across sessions because the kernel queue is process-wide. It owns
  queue `30000` and must attribute queued Echo Requests from source
  hardware-address metadata.
- The process-wide NAT66 UDP reply-socket registry exists because exact socket
  binds share the daemon's network namespace across MAC listeners and sessions.
  For its immutable daemon reply mark, it owns at most one live socket per exact
  IPv6 source address and port. Associations, per-listener DNS anchors, and DNS
  query tasks hold leases; only the last lease may remove and close the socket.
- A terminated TCP flow's outer idle deadline belongs to the flow record and is
  acted on by exact `(SocketHandle, worker)` identity, never by handle alone:
  smoltcp reuses handles, so a deadline or an activity wake matched on the handle
  would rearm or retire whichever flow took the slot. A flow whose token is
  already cancelled is excluded from the scheduled minimum and from expiry, because
  cancelling does not remove a flow - its worker completing does - so a passed
  deadline left in the schedule would wake the ingress task in a loop until that
  worker ran. Expiry begins a retirement and finishes none: the record, the socket,
  the channels and the charge come back through the same join fence every other
  ending uses, after `Workers::finished` says the descriptor is really gone.
- A flow is *begun* retiring exactly once. An idle expiry and a config retirement
  reach the same ending, and whichever is second skips a row whose per-flow token
  is already cancelled rather than discarding, aborting and counting it again -
  while still waiting for it, because the descriptor it holds is the one the
  acknowledgement is about. Neither walk collects what it is retiring: both step
  through the round-robin order every admitted flow is registered into, which
  holds each live handle exactly once.
- A flow may outlive its worker, and only in one direction: a clean terminal from a
  flow nobody cancelled, whose client side is past its handshake and not yet
  `Closed`, detaches the flow instead of removing it. The worker's descriptor is
  gone by then - its task ran to completion - and what remains is client-side
  state the ingress owner settles when smoltcp finishes, a floor runs out, a
  config retires it, or the session ends. It is removed and refunded exactly once
  whichever of those happens first, and no retirement ever waits for a second
  terminal from it.
- A flow's own physical endpoints are dropped before any grant covering them is
  released, and the mailbox is the one that makes this more than a formality: a
  transport cancelled inside its handover wait leaves a piece of an answer in that
  receiver, and that piece is charged to the delivery grant.
- On the app-UID path, the TUN ingress task owns every piece of client-keyed
  dataplane state, and it is the only reader of the descriptor. The session loop
  owns no such state, which is why the ingress task and not the session loop is
  what reports a retirement finished. The session loop does own both dataplane
  task handles and selects on their completion beside the control socket, so a
  dataplane that died ends the session rather than waiting for the app to speak;
  each handle is joined exactly once, by whichever of the two observed it.
- The nonfatal reporter belongs to exactly one conversation and is registered
  only weakly, so nothing can extend its life or install a second one beside it.
  Finishing it closes admission, flushes, waits for every producer already on its
  way to the writer, and joins its task, in that order; the registration stays
  busy until that has completed, so a successor cannot install into a teardown. A
  report afterwards is an explicit closed outcome that allocates nothing and
  revives nothing, and a report the writer's queue has no room for waits in its
  window rather than being queued or dropped.
- Every descriptor-bearing worker on the app-UID path is owned, never detached:
  UDP mapping and ping-socket receives, TCP flow upstreams, DNS-over-TCP
  transports, UDP resolver transactions, and the nonfatal reporter's window task
  all have a retained task handle. A DNS-over-TCP resolver transaction is owned
  without being a task at all - it is a row in a prepared, charged table the
  ingress owner polls, and dropping that row is what returns its descriptor. The same holds on the root path, where the only
  background task a call starts - the IPsec probe - is admitted to a
  process-owned set that the control loop cancels and joins before it stops
  session state, flushes its process-global reporter, or drops the sender that
  ends the control writer - so a probe cannot mutate retired state or report
  into a conversation that has already finished. Retained is
  not accumulated: a probe that has already finished is joined at the next
  admission, which is also where a probe that did not run to completion is
  reported, rather than at a process exit that may be hours away. Nothing terminal is sent as a message, because a
  message saying "closed" is sent while the sender still holds the descriptor.
  A worker's *completion* is the terminal event, and the owner joins it, drops
  the record it kept, refunds the budget, and only then lets the config
  acknowledgement go - in that order. Natural failure and explicit retirement
  settle through the same path, exactly once.
- A worker's waits must be preemptible by its own retirement. Writes,
  half-closes, reads, and event handovers all race the worker's cancellation
  token, because each of them is bounded only by a peer or by an owner that has
  stopped draining precisely in order to retire it.
- Retirement is two-phase: quiesce every affected producer, then join. A
  generation or epoch change retires the state keyed to it; whole-session EOF,
  cancellation, or failure retires all of it, so no daemon-owned task and no
  descriptor outlives the ingress task.
- Platform resolver work is not owned and cannot be joined. Dropping a query
  recovers this process's descriptor and nothing of the resolver's work, so the
  daemon's own accounting is exact for its own descriptors only, and neither
  retirement nor process death settles the platform's.
- No submitted resolver transaction is cancelled by a config change, on either
  transport, because cancelling reclaims nothing and destroys the completion the
  charge is keyed to. A transaction is therefore owned apart from the transport
  that asked for it, so a retirement neither waits for it nor cancels it, and its
  slot is refunded when the transaction itself reaches its terminal. Only the
  session ending cancels one.
- A resolver submission the platform accepted and this process cannot observe
  keeps its logical token, whether the observation was never possible -
  `android_res_nsend` succeeded and the local wrapper around its descriptor then
  failed - or was lost afterwards, when the readiness registration it was being
  watched with went away. The record and the bytes are refunded, the token is
  moved into a session-owned quarantine, and only the session ending releases it:
  refunding a slot Android is still holding would admit a query against a limiter
  with no room for one. No retry, deadline or restart is attached to it. A
  submission the platform *refused* is not this case and refunds its token with
  everything else, so which of the three outcomes a submission was stays typed
  until the owner that acts on it has read it.
- A quarantined token consumes no ledger row: it is moved onto a grant its owner
  already holds rather than split into one of its own, because the ledger is
  derived as one row per record-backed owner plus the statically known byte-only
  owners plus one spare for the single split in flight. So the move cannot be
  refused for capacity, and one release ends every token that took it.
- A closing DNS-over-TCP connection whose token did not reach the question it
  says is still outstanding does not have its grant released. The token would
  otherwise return to circulation while the platform's slot for that question is
  taken, so the grant goes back to the owner holding the quarantine instead - and
  if even that cannot account for it, the grant is kept and shows up as an
  outstanding lease in the exit report rather than as capacity handed back. The
  UDP handoff answers the same way for the same reason: a quarantine move it
  cannot represent leaves the query's own grant charged rather than releasing a
  slot Android may still be holding.
- An unobservable outcome is terminal for the stream that asked, whatever the
  generation says. The stale-generation replacement - a predecessor's answer
  becoming that query's own SERVFAIL on a connection that carries on - applies
  only to outcomes this daemon could actually observe, because it presumes a
  transport that still owns its logical token and may therefore ask again. A
  transport whose token has just been quarantined owns none, so replacing its
  refusal with a SERVFAIL would invite a retry it cannot make.
- A packet leaving on the app-UID path carries the upstream generation and
  downstream epoch it was produced under, and the writer drops it if either has
  since advanced. Producers gate on enqueue and the writer gates on dequeue,
  because neither alone catches work that crossed the boundary in flight.

## Interception

- Never install routing/firewall interception for a runtime capability that did
  not start successfully.
- DNS TCP and UDP listener ports are per-MAC, per-protocol capabilities. NAT66
  TCP and UDP listener ports are also per-MAC, per-protocol capabilities. NAT66
  ICMP Echo remains one session-level capability.
- Optional NAT66 TCP, UDP, or ICMP failure must not disable the other NAT66
  protocol capabilities that started successfully.
- ICMPv6 local-link control traffic is not upstream NAT66 payload.
- DNS, NAT66 TCP, and NAT66 UDP must carry MAC identity by entering through
  per-MAC listener resources. NAT66 ICMPv6 must carry MAC identity from a
  six-byte `NFQA_HWADDR`; source-IP-to-MAC lookup is not a valid fallback.
- Per-MAC listener ports are not client-provided identity tokens. Routing must
  reject or otherwise fail closed on direct listener-port access that bypasses
  the MAC-matched interception rule. NAT66 TCP/UDP listener ports are internal
  `::1` TPROXY endpoints.
- Local downstream traffic is outside the traffic-control boundary and must not
  be blocked or counted as upstream traffic by these mechanisms.

## Cleanup

- Cleanup must not depend on private app databases, preferences, caches, or
  daemon memory when the state can outlive the process.
- Session rollback may use the in-memory applied mutation list. Clean must
  reconstruct cleanup from deterministic identifiers and current system state.
- Missing state during cleanup is expected. Unexpected cleanup failures should
  be reported with structured context.
- Shared platform tables and globally owned platform state must not be flushed
  or disabled without an app-owned identifier.
- Adding a new persistent route, rule, address, firewall rule, mark, table, or
  chain requires adding or identifying its Clean path.
- Reply-socket leases are process-local descriptor ownership, not persistent
  routing or firewall state. Session stop and replacement release their leases,
  detached tasks may retain leases until completion, and process death closes
  any remaining descriptors without Clean bookkeeping.
- When a change mutates existing rules or similar, backwards compatibility is almost never considered since these mutations are cleared upon reboot.
  It is almost never worth the maintainability burden to carry over cleanup for legacy rules.

## Configuration

- Runtime packet/query work reads `SessionConfig` through snapshots and must
  not hold the config mutex while waiting on network I/O or resolver I/O.
  Session replacement may hold the mutex while routing is reconciled because
  that lock is the commit gate that keeps DNS/NAT66 readers on the previous
  config until the new routing state has committed.
- `SessionConfig.clients` is keyed by MAC. A client entry may have no IPv4
  addresses and still be a valid DNS/NAT66 authorization input.
- Session replacement reconciles routing before publishing the new config
  snapshot to NAT66/DNS readers.
- An empty client set is a deferred NAT66 state, not a failure. Later
  replacements may start NAT66 when clients appear.
- `ShizukuSessionConfig` is level-triggered: the newest config is the whole
  truth, neither axis may move backwards, and the daemon replays no history
  because the app coalesces to one pending slot. Admission is a config field,
  never inferred from an acknowledgement, and reopens only after the daemon has
  acknowledged the config that asked for it.

## Errors

- Terminal call errors mean the requested command could not complete.
- Nonfatal reports mean the daemon preserved the broader requested operation
  but lost an optional capability or observed unexpected background state.
- Per-MAC listener/routing failures are nonfatal when the daemon can omit only
  that MAC/protocol capability. Unattributable NAT66 ICMPv6 NFQUEUE packets are
  nonfatal background state and are dropped.
- Unexpected background failures in daemon-owned networking, resolver, netlink,
  routing, firewall, file descriptor, process, or cleanup work should become
  structured reports, not stderr-only logs.
- Cancellation is not an error by itself. Cleanup or channel failures observed
  during cancellation should be reported only when they affect daemon-owned
  state or indicate a broken invariant.

## Platform Assumptions

- Public app compatibility assumptions stay in the root README.
- Inline source comments should stay near hardcoded AOSP-derived values and
  hidden platform behavior.
- These docs should explain how the daemon depends on those assumptions without
  becoming a second hidden-API or platform compatibility index.
