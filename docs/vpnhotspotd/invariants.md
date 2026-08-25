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
