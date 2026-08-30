# Errors And Reports

Daemon reports distinguish a failed call from an operation that continued after
an optional or background failure.

## Report Shape

Reports built by
[`shared/protocol.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/protocol.rs)
carry context, message, optional errno, kind, Rust source location, process ID
and details. Context names the owner and operation. Details include the
identifiers needed to reproduce the failure, such as downstream/upstream,
session, client, destination, protocol, listener or queue.

Rust does not impose a second detail-count or field-length policy. The Android
consumer passes details to Crashlytics, whose
[event contract](https://firebase.google.com/docs/reference/kotlin/com/google/firebase/crashlytics/FirebaseCrashlytics#recordException(kotlin.Throwable,com.google.firebase.crashlytics.CustomKeysAndValues))
retains at most 64 combined app/event key-value pairs and truncates keys or
values beyond 1,024 characters. Crashlytics also retains only the
[eight most recent recorded nonfatal exceptions](https://firebase.google.com/docs/crashlytics/android/customize-crash-reports#report-non-fatal-exceptions).
Additional or longer diagnostic data is therefore handled at the consumer
boundary rather than silently discarded on the daemon wire.

## Control Framing And Diagnostic Bounds

Control protobufs use a four-byte length prefix. Both peers accept payloads up
to 2,147,483,643 bytes: `Int.MAX_VALUE` minus the four-byte prefix that the
app-UID descriptor handoff places in the same `ByteArray`. This is the largest
combined frame size arithmetically representable by `Int`. It remains below
[protobuf's portable 2 GiB ceiling](https://protobuf.dev/programming-guides/proto-limits/#total-size-of-the-message)
and is a structural framing maximum, not a daemon memory budget. Zero, negative
app-side lengths, and larger unsigned daemon-side lengths end the conversation
before payload allocation; allocation failure below the maximum remains Android
process-memory pressure.

Failed `iptables-restore`, `ip6tables-restore`, and `dumpsys ipsec` children can
write without bound even though one report cannot use unbounded diagnostics.
The daemon therefore retains the first 1,024 bytes from each stdout/stderr
stream. This is a conservative way to remain within Crashlytics' 1,024-character
value limit above: converting 1,024 source bytes cannot produce more than 1,024
Unicode characters, although multibyte UTF-8 may use fewer. Fixed 1,024-byte read
scratch buffers drain both child pipes concurrently. Firewall line consumers see
at most the same 1,024-byte prefix of any one line; `dumpsys ipsec` is passed to
its streaming parser in bounded chunks instead of first accumulating a complete
line. Bytes beyond a sample or line prefix are consumed and discarded, so hitting
the bound truncates only diagnostics/line inspection; the child cannot block on a
full pipe and command completion and status handling are unchanged.

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

Each process mode installs one reporter for its control conversation. Root's lasts
for the process and covers all calls, sessions and probes; the app-UID conversation
covers one session. A successor cannot install until its predecessor finishes.

Both modes coalesce by Rust source site `(file, line, column)`, bounding pending
categories to compiled report sites. Context, kind, errno, message, details and
the optional call ID remain payload; a summary carries the last blocked report
for its site.

Producers hand every unexpected failure to this shared owner rather than keeping
owner-lifetime "already reported" flags. This preserves each event whenever the
writer is available and leaves all suppression and latest-summary retention at
the documented source-site boundary.

The optional call ID only correlates a degradation with its owning call. Root can
report call-owned nonfatals after the call succeeds. On the app-UID path such a
failure is terminal and uses an `ErrorFrame`, so its nonfatal reports omit call IDs.

App-UID contexts begin with `shizuku.` and details describe only TUN-visible
source, destination and family. `platform_dns`, `platform_ipv4` and
`platform_ipv6` are traffic classes, not physical clients.

Only unexpected daemon/platform failures become app-UID reports. Malformed TUN
input, refused admission, expiry, unreachable peers and other ordinary
traffic-controlled outcomes are counted or logged, not reported per packet. A
ping socket delivering bytes that do not have the promised Echo-reply framing,
or returning `EMSGSIZE` without an attributable local path MTU, violates the
owner's expected kernel contract and is reported as well as counted.

## Coalescing And Delivery

The reporter reserves exactly one place in the serial control writer's queue:
one place is sufficient to keep one writer busy, while another would only move
backlog out of the coalescer. A report is attempted immediately whenever that
place is free. While it is occupied, each compiled source site retains only its
latest report and counts replaced reports in `coalesced.suppressed_count`.
Returning the place wakes the reporter, which emits blocked sites in
first-blocked order. There is no time-based reporting quota. Terminal frames are
not coalesced.

Orderly shutdown flushes summaries before ending the writer. Root first waits for
all report-capable detached tasks and their destructors; the app-UID session
finishes reporting before its terminal frame. Undelivered reports become part of
the conversation's result, and root combines that with the writer result. Reports
made outside a live conversation fall back to stderr/logcat.

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
affect daemon-owned state or break an invariant. An app-UID worker that owns a
descriptor releases its descriptor lease only after it completes, is joined and
the descriptor closes; clean TCP completion releases that upstream lease while
it may retain memory-only client-facing state as documented in
[`shizuku.md`](shizuku.md#app-uid-dataplane).

## Best-Effort Cleanup

Handle expected absence explicitly, such as a missing route, rule, address or an
already-closed descriptor. Report other cleanup failures with context. Do not
hide them with `let _ = ...`, `.ok()`, broad catches or stderr-only messages.
