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
- Netlink runtime owns the shared rtnetlink connection and event fan-out. It is
  created lazily on first netlink/routing use and then remains process-wide. It
  allows one neighbour monitor and one link monitor consumer at a time.
- The process-wide ICMP dispatcher exists only for NAT66 ICMPv6 state that must
  be shared across sessions because the kernel queue is process-wide. It owns
  queue `30000` and must attribute queued Echo Requests from source
  hardware-address metadata.

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
- An empty client set must not permanently disable NAT66. If process-wide
  firewall setup failed, NAT66 produced no runtime for a non-empty client set,
  or routing committed no NAT66 TCP/UDP capability for a non-empty client set,
  replacements keep NAT66 disabled for that session.

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
