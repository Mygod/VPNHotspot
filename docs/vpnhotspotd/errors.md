# Errors And Reports

Daemon reports distinguish a failed call from an operation that continued after
an optional or background failure.

## Report Shape

Reports built by
[`shared/protocol.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/protocol.rs)
carry context, message, optional errno, kind, Rust source location, process ID
and bounded details. Context names the owner and operation. Details include the
identifiers needed to reproduce the failure, such as downstream/upstream,
session, client, destination, protocol, listener or queue.

Preserve the first useful context when wrapping `io::Error`; generic layers must
not overwrite the failing site's context or source location.

## Terminal Errors

An `ErrorFrame` carries the call ID whose request could not complete. Examples
include malformed commands, duplicate calls or downstream sessions, missing
replacement targets, root downstream discovery failure, failed static-address
replacement and Clean failure.

On the app-UID path, start or dataplane failure answers the start call. A refused
configuration answers that config call and ends the session. The session cancels
and joins its dataplane and flushes nonfatal reports before the terminal frame,
which is the last frame written. A failure is delivered once: as the terminal
report when it ends the session, or as a nonfatal when the session continues,
never both.

## Nonfatal Reports

A `NonFatalFrame` is appropriate when the broader operation continues, for
example:

- one per-MAC DNS or NAT66 capability failed and was omitted;
- NAT66 ICMP lacked committed-client attribution and dropped one packet;
- one counter source failed while other counters remained available;
- daemon-owned background work or best-effort cleanup failed without invalidating
  the command.

The two modes share report builders but have separate registries:

| | Root | App-UID session |
| --- | --- | --- |
| Reporter | process-global | one per session, flushed before its terminal frame |
| Coalescing key | `(context, kind, errno, file, line)` | Rust source file, line and column, so attacker-controlled context cannot create unbounded categories |
| Nonfatal call ID | present when a specific active call owns the failure; otherwise absent | absent; call-owned failures use terminal `ErrorFrame`s with their `call_id` |

App-UID contexts begin with `shizuku.` and details describe only TUN-visible
source, destination and family. `platform_dns`, `platform_ipv4` and
`platform_ipv6` are traffic classes, not physical clients.

Only daemon-owned failures become app-UID reports. Malformed input, refused
admission, expiry, unreachable peers and other traffic-controlled outcomes are
counted or logged, not reported per packet.

## Coalescing And Delivery

The first report for a key is sent immediately. Further reports in the
one-second window are suppressed; the last becomes a summary with
`coalesced.suppressed_count` and `coalesced.window_ms`. A continuing category
emits at most one summary per window. Terminal frames are not coalesced.

Coalescing occurs before queueing, so at most one pending batch exists per key.
Orderly shutdown flushes summaries before ending the writer.
Delivery during disconnect is best effort; an undeliverable report falls back to
stderr/logcat. App-UID sessions account for undelivered reports in the failure
that ends the session.

## Local Setup Versus Remote Outcome

Classification belongs at the failing operation, not in an errno table:

- `Failure::Local` covers daemon setup such as creating, binding, making
  nonblocking or registering a socket, and wrapping a resolver descriptor. It is
  a structured report. A resolver-wrapper failure ends the app-UID session
  because later queries use the same broken facility.
- `Failure::Expected` covers peer, path and platform outcomes such as refusal,
  unreachability, timeout and ordinary resolver results. It ends only the flow or
  query and is not a report. DNS returns SERVFAIL for the affected message and
  keeps a valid TCP stream open.

See [`dns.md`](dns.md) for resolver ownership.

## Logs

`report::stdout!` and `report::stderr!` fall back to logcat after their pipe
closes. Use logs for expected remote outcomes and low-value summaries. Unexpected
daemon networking, resolver-wrapper, routing, firewall, netlink, descriptor,
process or cleanup failures must be structured reports rather than stderr-only
messages.

## Cancellation

Cancellation and `Interrupted` during cancellation are not errors. Tasks tied to
a stop token exit quietly. Report only unexpected cleanup/channel failures that
affect daemon-owned state or break an invariant. App-UID retirement releases a
worker's record and reservation only after it completes and is joined; clean TCP
completion may retain client-facing state as documented in
[`shizuku.md`](shizuku.md#app-uid-dataplane).

## Best-Effort Cleanup

Handle expected absence explicitly, such as a missing route, rule, address or an
already-closed descriptor. Report other cleanup failures with context. Do not
hide them with `let _ = ...`, `.ok()`, broad catches or stderr-only messages.
