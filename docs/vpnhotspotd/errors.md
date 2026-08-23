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

The app-UID path raises the same reports over its own frame. `ShizukuDaemonFrame`
carries either an applied acknowledgement or a `DaemonErrorReport`, and the app reads
that stream continuously rather than only while a config is in flight, because a
background failure arrives when it happens. Nothing else about the root envelope
applies there, and neither does root's reporter. The two share only the report
*builders*: root keeps its process-global channel and its `NonfatalCoalescer` keyed on
`(context, kind, errno, file, line)`, while an app session owns a reporter of its own,
site-keyed and flushed as part of the session's result, because their ownership and
failure semantics differ. Dispatch is registry-first with no fall-through, so neither
path's reports can reach the other's. A report from the app path carries no call ID,
because there are no calls at the app UID.

The app coalescer's key is the Rust source file, line and column and nothing else, which
is where it departs from root's. Context, kind and errno are traffic-controlled on this
path, so including them would let one forged packet per variation open one more window and
make "one batch per report site" a claim about nothing. The batch keeps the newest
occurrence, so its summary carries the latest context, kind and errno rather than the one
that opened the window.

Its contexts are prefixed `shizuku.` and name the owner and operation, such as
`shizuku.udp_send`, `shizuku.tcp_sweep`, `shizuku.tun_output`, or
`shizuku.echo_socket`. Details carry the safe tuple context - the TUN-visible source,
the destination, the family - and never label `platform_dns`, `platform_ipv4` or
`platform_ipv6` as a physical client, because none of them is one: every packet on
that path is untrusted input from an unknown local principal.

Packet input there is attacker-influenced, so what becomes a report is deliberately
narrow. A packet the daemon itself built and cannot send, a socket it cannot open, a
listen it cannot perform, a send that failed for no expected reason, a swept socket it
could not close abortively, and a receive task that could not report its close are
reports. Everything a client can drive at will - a malformed packet, a refused
admission, an expired hop limit, an unreachable remote - is counted and reported once
per epoch instead.

A terminated TCP flow reaching its outer idle floor is in that second group, and it is
worth naming because it is usually client-visible. A flow with a remote endpoint gets a
reset, which is the only way a terminated flow can say its state is gone, and that reset
is counted alongside every other one the engine writes. A flow with *no* remote endpoint -
one still listening, or one whose socket is already closed - has nothing for the stack to
answer with, so it is expired, counted and
cancelled in silence - which is why the engine's `expired` figure is kept apart from its
`reset` figure rather than being the same number under another name. Whether a reset that
was built reaches the wire is the writer's ordinary stamp gate: a config that changes the
stamp first purges it like any other packet of the retired stamp.

Neither outcome is a nonfatal report and neither may become one: how long a connection
idles is a client's choice, so a report per expiry would be a client-driven report stream,
and a flow taken back at its documented floor is the design working rather than something
going wrong. The relays that could still repeat under load report the first
occurrence and count the rest, and the coalescer bounds whatever is left.

## Local Setup Versus An Answer

For an operation that has both a local setup half and a remote half, which half failed
decides what the app is told, and an errno cannot say which one it was: an `EINVAL` from
a `setsockopt` on a socket this process just created is worth a report, while an
`ECONNREFUSED` from a connect is the ordinary answer to asking. So the classification is
made at the step that failed, as
[`shared/failure.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/failure.rs)'s
`Failure::Local { context, .. }` or `Failure::Expected`, and travels with the error.

- **Local** is the daemon's own setup: creating a socket, binding it to the selected
  `Network`, making it nonblocking, registering it with the runtime, or wrapping a
  resolver descriptor. Nothing a client can drive, so it becomes a structured, coalesced
  nonfatal naming the exact step - `shizuku.tcp_connect_socket`,
  `shizuku.tcp_connect_bind`, `shizuku.tcp_connect_nonblock`,
  `shizuku.tcp_connect_register`, `resolver.nonblock`, `resolver.register` - with the
  record's own details beside it.
- **Expected** is what the peer, the path or the platform answered: refused, unreachable,
  timed out, or the resolver's own outcomes. That is an ordinary per-flow or per-query
  outcome and never a report, because a client chooses how many flows it opens and how
  many names it looks up. A terminated TCP flow's client learns of it from the reset the
  engine writes; a DNS client learns of it from SERVFAIL.

Terminated TCP therefore no longer maps a whole connect attempt to one stdout line: the
peer's answer still is one line per flow, and only a local setup failure is structured.
The app-UID resolver keeps the same split - `EBUSY` from the platform's per-UID limiter,
timeouts, and every other platform or remote outcome stay SERVFAIL and expected, while the
two wrapper steps around a transaction are the daemon's own. Root's DNS proxy is untouched
and keeps master's own resolver, readiness and error path.

For DNS a resolver outcome is not a *stream* outcome either. An expected outcome becomes a
correctly framed SERVFAIL for that one message and the connection carries on, in every mode:
a reset per unresolvable name would end a stream the client is entitled to keep using, and
one stderr line per unresolvable name would be a log flood any local app could drive by
looking up nonsense, so an expected resolver outcome is silent on all three paths. Only a
local wrapper failure - or a query too malformed for a SERVFAIL to be built from, which has
no question to echo back - ends a DNS-over-TCP flow, and only the local one is structured.
DNS-over-TCP is unchanged in every other respect: its transaction is still owned separately
from its transport - as a row in the ingress owner's own table rather than as a task - and
still settles when the platform is actually done. Neither the local failure nor the malformed
query parks a delivery: a value no acknowledgment could ever name would be a grant that could
only end when the whole connection closed, so it is classified before anything is parked and
what ends the stream is the refusal the transport receives.

Four more outcomes on that transport are answers rather than failures, for the same reason:
a query accepted while no `Network` was selected - including one on a connection opened
before any config had selected one, which such a transport does not need - one the descriptor
floor had no room for, one the transaction table would have had to grow for,
and one whose answer came back from a selection the session has since left. Each is that
message's own SERVFAIL on a connection that carries on. A query the aggregate cannot admit at
all is the exception and is dropped rather than answered - there is nothing to echo, since
nothing of it was stored - and the transport skips exactly the bytes its length announced so
the stream stays framed. None of the four is reported: a client chooses how many queries it
sends.

Nonfatal reports are coalesced before they are sent to Kotlin. The first report
for a category is emitted immediately. Further reports with the same context,
kind, errno, and Rust source file/line are suppressed for the current
one-second window; when the window closes, the daemon emits the last suppressed
report with `coalesced.suppressed_count` and `coalesced.window_ms` details. If
reports continue, subsequent windows keep emitting at most one summary per
second instead of reopening with another immediate report. If a category goes
quiet for a full window, the pending batch closes and the next report is again
emitted immediately. An orderly root shutdown flushes pending summaries into the
process-global reporter's channel before it drops the sender that ends the control
writer, so a summary made by the last thing to stop still leaves. That order does not
hold on the other way out: a writer that fails closes and drains its own queue, cancels
the conversation and is about to return, so the cancellation it raises is what drives the
probe join and the session teardown that follow - and reports raised during those have
nowhere left to go. Nonfatal
control-frame delivery is best-effort during controller disconnect or idle shutdown
either way. If an emitted nonfatal cannot be
written to the control socket, the daemon writes the report to stderr, which
falls back to logcat if the app-side stderr pipe is already closed. Terminal
error frames are not coalesced.

Coalescing happens on the producer's side, before anything is queued, and that is a
resource property rather than a style choice: reports are made from the packet paths, so a
queue in front of the coalescer would let one forged packet per report allocate a report
nobody is draining. What exists at any moment is therefore one pending batch per distinct
report site - a count fixed by the daemon's source, not by traffic - and the only thing
left for a task is closing a window that nothing else would wake for.

Coalescing bounds what a *client* can make the daemon hold; a fixed number of places in the
control writer's queue bounds what a slow *controller* can. That queue is unbounded, because
a reply or an event a session must not lose can always be put on it, so reporting owns
exactly **one** place in it and hands each report that place; it comes back when the writer
drops the message it wrote. One rather than a round number: the writer is serial, so a
second handed report could not be written any sooner than the first, and a larger share
would only move reports out of the coalescer - where they are summarised - into a queue
where they are not. A report that finds no place free is not dropped: it stays in its window
as one more occurrence, so the summary that closes the window delivers it, and what is
waiting is still one batch per report site however long the controller stops reading. The
window then closes on the place coming free rather than on a timer, so nothing polls for it.

Note what a place returning does *not* prove. A report the sink accepted is on the writer's
queue; that its bytes reached the peer is proven by the session joining the writer task,
which is a separate step. Reporter finalization on its own proves only that nothing more
will be handed over.

The reporter belongs to one conversation, and terminally. It is *installed* by the
conversation that owns it and registered only weakly, so being reachable from every packet
path is not being kept alive: a strong global would hold the control writer's sender past
the point its owner dropped it, and the session would never see the writer's own result.
Only one installation may be live, so two overlapping conversations cannot each believe
they own reporting - and because the registration happens before the window task is
started, a refused installation has started nothing to leak.

Ending it is a **task**, not a `Drop`, because every step of it has to wait for something.
One finalizer exists per reporter; both `finish` and the guard's `Drop` do nothing but make
sure it has been started, and only `finish` awaits its outcome. It closes admission and
extracts what the coalescer still holds under the admission lock, cancels, then - outside
the locks - drains every producer already between the admission check and the sink, joins
the window task, and hands the last summaries over one at a time, waiting for the writer to
have taken each before offering the next. Only then does it store the outcome, release the
registration and wake the waiters. Ordinary lookup ends the moment finalization begins,
while the registration stays *busy* until the finalizer has completed, so a successor
conversation cannot install its own reporter while the predecessor's producers are still
draining and its task is still being joined. A conversation that returns without finishing -
a setup step that failed after the installation - starts the same finalizer; dropping a
`finish` future part way through gives up the answer and nothing else.

Two consequences are stated rather than papered over. A controller that has stopped reading
and never disconnects keeps the finalization, and therefore the registry, closed forever;
the alternative is a timeout that calls a report delivered while it sits in a queue nobody
is draining. And a guard dropped where no tokio runtime can own the finalizer closes
admission and cancels the window task but neither joins nor detaches it and never releases
the registration, so that process refuses every later installation - fail-closed, and
visibly so.

The final flush is the one place a report can be lost rather than kept: there is no later
window to hold it in, so a report the writer will not take is counted undelivered and
becomes the session's failure.

A report made before a reporter is installed or after one has finished is an expected
no-op with an explicit outcome: it is handed back to the caller and written to stderr,
never coalesced, never queued, and never able to open a window or revive a task.

If an emitted nonfatal cannot be written to the control socket, the daemon writes the
report to stderr, which falls back to logcat if the app-side stderr pipe is already closed
- there is nowhere else for a report about the report path to go. The *loss* is not
swallowed with it: the reporter counts undelivered reports, and a window task that did not
run to completion or a report that could not be delivered becomes the session's own
failure, which is the exit status the app reads.

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

On the app-UID path a worker says which of the two its ending was, and its owner acts on
that rather than guessing. A worker the owner retired, or one whose exchange ended the way
its protocol says it ends, is silent. A peer that reset, timed out, or became unreachable is
one log line per record - never per packet, since a client chooses how many records there
are. Anything else is the daemon's own I/O or its own task failing, and becomes a structured
report carrying the operation's context, its errno, and the record it belonged to. A worker
task that did not run to completion is reported that way too, and its record is settled
anyway: the task is gone either way, and leaving the record would strand its charge.

## Best-Effort Cleanup

Best-effort cleanup should handle expected benign absence explicitly, for
example missing routes, missing rules, missing addresses, or already-closed
file descriptors. Other errors should be reported with context.

Do not hide cleanup errors with `let _ = ...`, `.ok()`, broad catches, or
stderr-only messages. If a cleanup operation is intentionally allowed to fail,
document the expected failure mode in code or in the owning doc.
