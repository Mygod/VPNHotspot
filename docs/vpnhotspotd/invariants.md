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
- On the app-UID path the TUN ingress task owns every TUN-visible flow, mapping
  and transport, and is what reports when retirement has finished. The session
  loop owns the two dataplane task handles, joins each exactly once, and ends the
  session as soon as either completes.
- A terminated TCP flow is identified by its socket handle and worker together,
  never by the handle alone, because smoltcp reuses handles.
- A terminated TCP flow's worker crosses the boundary to the ingress owner as a
  bounded Tokio `AsyncRead + AsyncWrite` stream and nothing else. That stream is
  three one-way library pipes joined and chained, all three charged to admission
  before the flow exists, and the worker cannot tell how many there are. Both directions
  are bounded by that stream's own buffer and both wakes are the library's: the
  worker writing is what makes the owner runnable, and the worker reading is what
  returns the capacity the owner was waiting for. Neither direction may be
  governed by reading a capacity, a timer, a periodic poll, an unrelated event or
  a readiness message, and neither may travel on anything other flows share.
- The ingress owner moves bytes straight between that stream and `smoltcp`'s own
  buffers, so nothing is ever held between the two. What the client's send buffer
  will not take stays in the stream, which stops the worker reading its upstream;
  what the stream will not take stays in the client's receive buffer, which closes
  the client's window. Neither direction may drop a byte to relieve pressure.
- The owner does not read the stream while the client's send buffer is full, and
  does not write it while the client's receive buffer is empty. Both of those are
  refilled only by a packet or a stack timer, which the owner is already woken
  for and after which it re-enters the crossing immediately.
- One pass gives every live flow exactly one bounded turn, and *ending* the pass
  is what rotates the order, so no flow can hold the pass against the others,
  none can be skipped by one that is busier, and none is first for ever. That
  order is one type: admission takes a place in it, a refused candidate takes back
  exactly the place it took and never a predecessor's, and a reclaimed flow takes
  its own. There is no second path into it.
- The client's half-close reaches the worker only after every byte the client sent
  before it, and the daemon never *waits* for a full stream to drain to arrange
  that. On the **ingress that carried the FIN, before that call returns**, the
  main stream is closed and the remaining receive bytes are moved into a
  pre-created, admission-charged one-way tail whose capacity is asked of the
  socket rather than read from a field beside it; the tail is closed once the
  buffer is empty, and the worker - reading `down.chain(tail)` - observes main
  bytes, then tail bytes, then one end of stream. Closing the main stream first
  is what orders the two, so no later byte can overtake one already in the tail.
- Nothing may poll the stack between a client's FIN arriving and its ending being
  taken out. The owner's other arms each poll it - a configuration change can
  await a whole retirement, a terminal reclaims, a deadline fires - and a due
  `TIME-WAIT` socket clears its receive buffer inside any of them. The seal is
  therefore on the exact flow the packet named, on that packet's own call, and
  covers only that flow's terminal extraction. The order within that call is
  fixed, and two of its polls are conditional: a settle *before* the push only when
  a reset names a flow whose phase one could actually move - an *unknown* reset or
  an *unknown* `SYN|RST` names none and pays the ordinary poll only, while a
  `SYN|RST` matching a reachable flow is a candidate like any other; then push;
  then settle; then classify an accepted reset and fence it, or arm the idle floor
  and extract the ending; then settle again only if that left the stack something
  to do. The Closed-socket scan that may cancel a worker runs after all of it,
  never inside the settle primitive, so no decision above can be pre-empted by one.
  It runs once per packet the stack saw; a `SYN` admission refused before anything
  was built returns first, having changed nothing to scan.
- If the tail ever could not take an ending, both pipes are closed anyway so the
  worker's stream still ends, the socket is fenced, its worker cancelled, and it
  is counted apart from every ordinary ending and reported as a structured
  non-fatal. A worker's reader merely disappearing is not that: it races that
  worker's own terminal and is never reported.
- A `simplex` half signals nothing on drop. Every end of stream this daemon
  reports is an explicit shutdown - by `copy_bidirectional` when its reader ends,
  by a DNS-over-TCP transport, or by the owner's `Bridge` closing both of its
  write halves.
- That extraction is **one uninterruptible step**, never a state the flow can be
  caught half-way through. The owner runs `Interface::poll` between crossings from
  five paths, and a `TIME-WAIT` socket clears its receive buffer inside one, so a
  half-done extraction is acknowledged bytes waiting to be discarded. Tokio's
  cooperative budget is the only thing that could split it and is removed for
  exactly that step by `tokio::task::unconstrained` - one shutdown, at most two
  writes over the receive ring's contiguous runs, one shutdown. Only an empty
  receive buffer closes the tail, asked of the stack. With the budget gone a
  refusal can only mean a tail built smaller than the receive buffer, which is a
  construction error and ends the flow abortively rather than truncating a clean
  close.
- The daemon's own FIN is never emitted from `CLOSE-WAIT` while `can_recv` is true,
  so a socket cannot reach `LAST-ACK` or `Closed` holding client payload nothing
  can read. Asked of the stack, not of any daemon state. From `ESTABLISHED` an
  upstream EOF is still propagated at once, so the two half-closes stay
  independent.
- Extraction beats smoltcp's fixed ten-second close timer - which `set_timeout`
  does not govern and whose expiry clears the receive buffer - by leaving no
  half-done state for a poll to catch, not by scheduling. Biasing the ingress
  `select!` toward `tcp.attention()` decides only which arm is offered first;
  every path that arm reaches polls the stack itself.
- A client's abort is what the *stack* accepted, never what a header flag said.
  The `RST` bit is a candidate; the accepted same-instant transition is the cause.
  The owner carries no reset cause: it records which flow a reset segment names
  and what phase that socket is in, lets the packet be pushed and processed, and
  ends the flow abortively only if that exact socket then transitioned the way an
  accepted reset makes it - to `Closed`, or from `SYN-RECEIVED` back to `LISTEN`.
  An accepted reset aborts that socket **synchronously**, before the worker is
  cancelled, so a same-tuple `SYN` cannot attach to a listener that reclamation is
  about to destroy.
  Sequence, window and checksum validation are the stack's and are not
  reimplemented. Everything already due is settled first and both polls run at one
  instant, so a due close timer cannot be read as a reset; a socket already
  `Closed` is never a candidate; and a packet the device refuses changes nothing,
  because there is no cause to change. Which flow a segment names is both of its
  endpoints in the client's own direction, and is one rule shared with the
  decision to open a flow for a SYN.
- A client-side socket the stack has finished with cancels its worker, except when
  the client half-closed cleanly and the extraction completed: that worker may still be flushing bytes this
  daemon acknowledged, and cancellation is abortive. Such a flow ends on its
  worker's own completion, and stays bounded by its idle floor, by any retirement
  and by session shutdown - all of which remain abortive. A reset, a flow that
  never opened and a worker already gone are not this case.
- A clean client ending is bounded from the moment the *stack* sees the FIN, and
  the idle floor is armed **before** the extraction changes what the phase means -
  in the same synchronous ingress call, after the poll that produced the phase and
  with nothing awaited in between. A sealed flow is flushing, and a flushing flow
  in a terminal phase preserves the deadline it already has, so sealing first
  would preserve the *previous* one and let a FIN that arrived just before it
  expired be cancelled mid-flush. At rearm time the flow is still pending, so it
  takes the established floor: the bytes the client sent are still on their way
  through this daemon however far the client-facing half has torn down, and the
  phase there can be `TIME-WAIT`, whose own floor is none. An accepted reset is
  not rearmed. A flow may never come out of that window unbounded.
- Once the halt is propagated, a phase that can still put application data in
  front of the client - `ESTABLISHED`, `CLOSE-WAIT` - is rearmed by packets and
  deliveries like any other active flow; freezing it expires a response that was
  never idle. Only the terminal phases, where no byte can reach the client, keep
  the finite deadline the flush already has, because their own floors are none or
  zero and either would unbound the flush or make it immediately due.
- No figure in any of the above is invented for the purpose: every one is an
  existing RFC 5382 floor, applied to the phase whose behaviour it describes.
- A clean terminal may arrive with bytes still in the stream, so a flow is
  detached rather than removed: what the worker wrote stays readable and the end
  of the stream it shut its write half down for follows it, and the owner goes on
  delivering. Only a flow that is ending discards what it still owed.
- A flow begins retiring once, and its record is removed and its budget
  reservation released once, whichever of an idle timeout, a configuration change
  or the session ending reaches it first. A flow whose worker has already
  finished is settled directly, and nothing waits for a second terminal from it.
- Nothing a worker holds is released before that worker has finished, and the
  owner learns that by joining the task rather than by receiving a message: it
  cancels, joins, closes and only then returns the reservation, so an
  acknowledged configuration means those descriptors are closed.
- Every descriptor-bearing worker is owned rather than detached, on both paths.
  Root's IPsec probe is cancelled and joined before the control loop stops session
  state or ends the control writer.
- A worker's waits must be interruptible by its own retirement, since each is
  otherwise bounded only by a peer.
- Platform resolver work is not owned and cannot be joined, so the daemon's
  accounting is exact for its own descriptors only. No submitted transaction is
  cancelled by a configuration change, and a transaction Android accepted but
  this process can no longer observe keeps its resolver slot reserved until the
  session ends; see [`dns.md`](dns.md).
- The nonfatal reporter belongs to exactly one conversation, and a successor
  cannot install one while its predecessor is still finishing.
- A packet leaving on the app-UID path carries the `upstream_generation` and
  `downstream_epoch` it was produced under, and is dropped if either has since
  advanced.

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
- `ShizukuSessionConfig` is level-triggered: the newest configuration is the
  whole truth, neither `upstream_generation` nor `downstream_epoch` may move
  backwards, and no history is replayed. Admission is a configuration field,
  never inferred from an acknowledgement.

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
