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
- Netlink runtime owns the shared rtnetlink connection and event fan-out. It
  allows one neighbour monitor and one link monitor consumer at a time.
- The process-wide ICMP dispatcher exists only for NAT66 ICMPv6 state that must
  be shared across sessions because the kernel queue is process-wide.

## Interception

- Never install routing/firewall interception for a runtime capability that did
  not start successfully.
- DNS TCP and UDP listener ports come from DNS startup independently. NAT66
  TCP, UDP, and ICMP Echo capability come from NAT66 startup independently.
- Optional NAT66 TCP, UDP, or ICMP failure must not disable the other NAT66
  protocol capabilities that started successfully.
- ICMPv6 local-link control traffic is not upstream NAT66 payload.

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

- Runtime work reads `SessionConfig` through snapshots. Do not hold the config
  mutex while waiting on network I/O, resolver I/O, or external system
  commands.
- Session replacement reconciles routing before publishing the new config
  snapshot to NAT66/DNS readers.
- If NAT66 produced no runtime during session startup, replacements keep NAT66
  disabled for that session. Replacement does not retry NAT66 startup.

## Errors

- Terminal call errors mean the requested command could not complete.
- Nonfatal reports mean the daemon preserved the broader requested operation
  but lost an optional capability or observed unexpected background state.
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
