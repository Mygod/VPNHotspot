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

Tie the report to a call ID when the failure belongs to a specific call.
Use process-level nonfatal reports only for daemon-global background failures or
when no meaningful call owns the failure.

The app-UID path raises the same reports in the same `DaemonEnvelope`. The app
reads that stream continuously rather than only while a configuration is in
flight, because a background failure arrives when it happens rather than in reply
to anything.

An app-UID failure that belongs to a call is not a nonfatal at all: the start call
is answered with an `ErrorFrame` for anything that refuses the descriptor or the
dataplane and for whatever ends the session, and a configuration call with one for
whatever refuses it ([`lifecycle.md`](lifecycle.md#app-uid-session-start)). Exactly
one of those two calls is answered per session, and that answer is the session's
last frame.

Each failure has exactly one destination, chosen by the session at teardown: the
terminal frame this session owes when nothing has claimed it yet, nothing when the
failure is the one that frame already carries, and a nonfatal otherwise. Owners do
not emit their own reports - the TUN writer and the session seed attach a report to
the error they return and leave the delivery to the session - because emitting at
the owner as well would make one fatal failure arrive twice, once as a
`NonFatalFrame` and once as the `ErrorFrame` carrying the identical report.
Whichever destination is used, the report delivered is the one the failing site
built, so the errno, the details and the Rust source location are its own. What
the dataplane reports in the background carries no call ID, because packet work is
not owned by a call.

Every terminal frame of an app-UID session is enqueued after that session's
reporter has finished, and is the last daemon-to-app frame the session writes.
That holds for a refused configuration's `ErrorFrame` exactly as it does for the
start call's: the app's reader returns on the first terminal frame it sees, so a
nonfatal enqueued behind one is a nonfatal the app never reads. Refusing a
configuration therefore describes the failure where the check happened - which is
what keeps its context, errno, details and Rust source location - and leaves the
frame to the session's teardown, so the reports the teardown itself discovers and
the summaries still held in a coalescing window both go out first. A report raised
after the reporter has finished has nowhere left to go and is written to stderr
rather than enqueued behind the terminal frame.

The two paths share the report builders and differ in delivery and keying:

| | Root | App-UID session |
| --- | --- | --- |
| Owner | one process-global channel and coalescer | one reporter per session, flushed as part of its result |
| Coalescing key | `(context, kind, errno, file, line)` | Rust source file, line and column only, because context, kind and errno are traffic-controlled there and would otherwise let one forged packet per variation open another window |
| Call ID | carried when the failure belongs to a specific call; otherwise absent | absent; call-owned failures use terminal `ErrorFrame`s |

Dispatch is registry-first with no fall-through, so neither path's reports can
reach the other's. App-UID contexts are prefixed `shizuku.` and name the owner and
operation, such as `shizuku.udp_send`, `shizuku.tun_output`,
`shizuku.handoff.tungetiff` or `shizuku.control.config`. Details carry the
TUN-visible source, the destination and the family, and never label
`platform_dns`, `platform_ipv4` or `platform_ipv6` as a physical client, because
none of them is one.

Because packet input there is attacker-influenced, only the daemon's own failures
become reports: a packet it built and cannot send, a socket it cannot open, a
listen it cannot perform, a send that failed for no expected reason, a retired
socket it could not close abortively, or a receive task that could not report its
close. Everything a client can drive - malformed packets, refused admission,
expired hop limits, unreachable remotes, and a terminated TCP flow reaching its
idle floor - never becomes a structured report: it is counted and summarized in
the stdout diagnostics the daemon writes once at session exit, where the ingress
counters run for the session's whole life because no configuration change divides
the client traffic they count. A generation change writes its own per-owner
diagnostics after the retirement it caused has completed.
Expiry counters stay separate from reset counters, since a flow with no remote
endpoint is retired silently.

Nonfatal reports are coalesced before they are sent to Kotlin. The first report
for a category is emitted immediately. Further reports with the same key are
suppressed for the current one-second window; when the window closes, the daemon
emits the last suppressed report with `coalesced.suppressed_count` and
`coalesced.window_ms` details. If reports continue, subsequent windows keep
emitting at most one summary per second instead of reopening with another
immediate report. If a category goes quiet for a full window, the pending batch
closes and the next report is again emitted immediately. Terminal error frames are
not coalesced.

Coalescing happens before anything is queued, so what a report flood can make the
daemon hold is one pending batch per report site - a count fixed by the daemon's
source rather than by traffic - however slowly the controller reads.

If an emitted nonfatal cannot be written to the control socket, the daemon writes
the report to stderr, which falls back to logcat if the app-side stderr pipe is
already closed. The loss is still accounted for: an app-UID session counts
undelivered reports and folds them into the failure that ends it. What the app
observes is the report frames that were delivered, plus control-stream failure
or closure; the child's exit code is only logged.

An orderly root shutdown flushes pending summaries before it ends the control
writer, so the final summary is still sent even when the last thing to stop is
what produced it. Delivery is
best-effort during controller disconnect or idle shutdown, and reports raised
after a writer failure have nowhere to go, since that failure is what ends the
conversation.

## Local Setup Versus An Answer

For an operation that has both a local setup half and a remote half, which half
failed decides what the app is told, and an errno cannot say which one it was:
an `EINVAL` from a `setsockopt` on a socket this process just created is worth a
report, while an `ECONNREFUSED` from a connect is an ordinary per-flow result.
The classification is therefore made at the step that failed, as
[`shared/failure.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/failure.rs)'s
`Failure::Local { context, .. }` or `Failure::Expected`, and travels with the
error.

- **Local** is the daemon's own setup: creating a socket, binding it to the
  selected `Network`, making it nonblocking, registering it with the runtime, or
  wrapping a resolver descriptor. A client cannot drive any of those, so such a
  failure becomes a structured report naming the exact step -
  `shizuku.tcp_connect_socket`, `shizuku.tcp_connect_bind`,
  `shizuku.tcp_connect_nonblock`, `shizuku.tcp_connect_register`,
  `resolver.nonblock`, `resolver.register` - with the record's own details
  beside it. A per-flow one is a coalesced nonfatal and the flow ends. The two
  resolver ones and the readiness registration behind them are wider than one
  query: `Failure::ending` turns them into the error the app-UID ingress task
  returns, so the session ends and `shizuku/app_session.rs` delivers that one
  attached report through its single routing point - the start call's terminal
  error frame when nothing has claimed it, a nonfatal otherwise. That is the
  *first* such failure only. Any further one observed independently - every
  outstanding query fails the same way if the runtime's I/O driver goes - has no
  result left to travel on, so the DNS drain and shutdown path emits it locally
  as a nonfatal through `report::keep_first`, and a query task whose owner is
  already gone emits its own the same way. Each failure therefore takes exactly
  one of the two routes and never both; see [`dns.md`](dns.md) for which.
- **Expected** is what the peer, the path or the platform answered: refused,
  unreachable, timed out, or the resolver's own outcomes. That is an ordinary
  per-flow or per-query outcome and never a report, because a client chooses how
  many flows it opens and how many names it looks up. A terminated TCP flow's
  client learns of it from the reset the engine writes; a DNS client learns of
  it from SERVFAIL.

An expected outcome is also never a *stream* outcome: a resolver answer becomes a
framed SERVFAIL for that one message and the connection carries on. A query too
malformed for a SERVFAIL to be built from ends that DNS-over-TCP flow, and a
local resolver wrapper failure ends more than the flow - it ends the session that
owns the resolver, as above. [`dns.md`](dns.md) owns which resolver outcomes fall
on which side.

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
that token is cancelled. DNS TCP accept also re-checks its stop token before
reporting accept errors and retries transient active-listener errors.

If cancellation exposes an unexpected cleanup or channel failure, report that
failure only when it affects daemon-owned state or indicates a broken invariant.

On the app-UID path a worker reports which kind of ending it had. A retirement or
an ordinary protocol end is silent; a peer that reset, timed out or became
unreachable is one log line per record, never per packet; anything else, including
a task that did not run to completion, is a structured report. Whichever it was,
its completion is what permits the record to be removed and the budget
reservation released. That is immediate except in one case: a terminated TCP flow
whose transport task completed cleanly while the client's own connection was
still open keeps its record and its reservation until that client-side close
finishes, or until an idle floor, a configuration retirement or session shutdown
reaches it first
([`shizuku.md`](shizuku.md#transport-completion-and-client-side-close)).

## Best-Effort Cleanup

Best-effort cleanup should handle expected benign absence explicitly, for
example missing routes, missing rules, missing addresses, or already-closed
file descriptors. Other errors should be reported with context.

Do not hide cleanup errors with `let _ = ...`, `.ok()`, broad catches, or
stderr-only messages. If a cleanup operation is intentionally allowed to fail,
document the expected failure mode in code or in the owning doc.
