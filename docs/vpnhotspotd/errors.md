# Errors And Reports

Daemon errors need enough structure for the Kotlin side to show useful
diagnostics, attach Crashlytics keys, and distinguish terminal call failures
from background nonfatal reports.

## Report Shape

Structured reports are built in
[`shared/protocol.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/protocol.rs)
and sent by
[`report.rs`](../../mobile/src/main/rust/vpnhotspotd/src/report.rs). A report
contains:

- context string;
- message;
- optional errno;
- kind;
- Rust source file, line, and column from `track_caller`;
- daemon process ID;
- bounded key/value details.

Use context strings that name the owning subsystem and operation, such as
`routing.start`, `control.replace_session`, or `nat66.udp_connect`. Details
should include the concrete identifiers needed to debug the failing operation,
such as downstream interface, upstream interface, session ID, client, or
destination. Per-MAC admission and accounting failures should include the MAC,
downstream interface, protocol/source, and queue number or listener port when
that state exists.

## Terminal Call Errors

A terminal call error is sent as an error frame for the call ID. Kotlin converts
it into `DaemonException` and completes the matching one-shot reply or closes
the matching event channel.

Use terminal errors when the requested operation cannot be completed:

- malformed control frames or command payloads;
- duplicate active call ID;
- duplicate session for a downstream;
- missing session for replacement;
- downstream IPv4 discovery failure during session start;
- static-address replacement failure;
- Clean failure that prevents the command from completing.

When adding context to an `io::Error`, use the report extension helpers so the
first useful daemon context is preserved and not overwritten by generic wrapper
layers.

## Nonfatal Reports

A nonfatal report is sent independently of the terminal frame path. Kotlin logs
it and shows an app-visible warning. Nonfatal reports are appropriate when the
daemon preserves the broader requested operation but loses an optional behavior
or observes unexpected background state.

Representative examples:

- a per-MAC DNS or NAT66 listener/routing capability fails, and the daemon omits
  only that MAC/protocol capability;
- NAT66 ICMPv6 receives a packet without usable committed-client attribution and
  drops that packet while preserving the broader NAT66 session;
- IPv4 forwarding counter readout fails during a traffic-counter read, while
  daemon-owned DNS/NAT66 counters can still be returned;
- a background task or best-effort cleanup step fails without invalidating the
  command's main result.

Tie the report to a call ID when the failure belongs to a specific active call.
Use process-level nonfatal reports only for daemon-global background failures or
when no meaningful call owns the failure.

## Logs

`report::stdout!` and `report::stderr!` write to stdio, falling back to logcat
if the stdio pipe is already closed. Use logs for expected remote/network
outcomes and low-value runtime noise that should not surface as a structured
app warning.

Do not use stderr-only logging for unexpected daemon failures in networking,
resolver, routing, firewall, netlink, fd, process, or cleanup operations.
Unexpected background failures should become structured nonfatal reports.

## Cancellation

Cancellation is not an error by itself. If a call is cancelled and the failing
operation returns `Interrupted`, the call task should finish without sending a
terminal error. Background tasks tied to a stop token should exit quietly when
that token is cancelled.

If cancellation exposes an unexpected cleanup or channel failure, report that
failure only when it affects daemon-owned state or indicates a broken invariant.

## Best-Effort Cleanup

Best-effort cleanup should handle expected benign absence explicitly, for
example missing routes, missing rules, missing addresses, or already-closed
file descriptors. Other errors should be reported with context.

Do not hide cleanup errors with `let _ = ...`, `.ok()`, broad catches, or
stderr-only messages. If a cleanup operation is intentionally allowed to fail,
document the expected failure mode in code or in the owning doc.
