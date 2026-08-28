# Lifecycle

The binary has two entry contracts in
[`main.rs`](../../mobile/src/main/rust/vpnhotspotd/src/main.rs): a socket name
alone selects the root path, while `--app-uid` followed by a socket name selects
the Shizuku path. They share the APK library lookup, the linker invocation and
the frame format, and little else - routing, firewall, `ndc` and NFQUEUE
mutations exist only on the root path, because none of them is permitted at the
app UID.

**Root mode.** `vpnhotspotd` is started lazily by
[`DaemonController`](../../mobile/src/main/java/be/mygod/vpnhotspot/root/daemon/DaemonController.kt)
when the app sends the first daemon command, and stays alive only while the
controller has active calls. When the last call is closed, Kotlin closes the
control connection; the Rust control loop then stops all daemon-owned runtime
state and exits. Everything from [Calls](#calls) onwards describes this path.

**App-UID mode.** The child is launched by
[`AppUidDaemon`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/AppUidDaemon.kt)
and speaks the same call conversation over a command family of its own: one
session per connection, and no root mutations. Its session is started by the call
below, and the feature it serves is described in [`shizuku.md`](shizuku.md).

## Process Startup

Kotlin locates the native `vpnhotspotd` library in the APK and runs it through
Android's linker from a root command. It creates:

- an abstract Unix-domain server socket name for the control channel;
- stdout and stderr pipes that are drained into Timber;
- one root command invocation that starts the daemon with the socket name.

Started that way, the daemon receives exactly one argument: that socket name. It
connects back to the abstract Unix socket, splits the stream, starts a writer task
for outbound frames, installs the one nonfatal reporter for the conversation, and
builds process-wide bookkeeping:

- one NAT66 ICMP dispatcher shared by NAT66 sessions and bound to the
  app-owned NFQUEUE number `30000`;
- one NAT66 UDP reply-socket registry shared by all MAC listeners and sessions,
  using the process's immutable daemon reply mark;
- a session map keyed by the start-session call ID;
- one process-wide upstream-interface aggregate for optional IPsec probes,
  beside the owner that holds those probes' task handles;
- one optional neighbour monitor;
- one process-wide flag for the NAT66 firewall base chains.

Each notification consumer owns a multicast-only rtnetlink connection and a
separate request connection where needed. Session routing retains its request
connection for the session lifetime; one-shot commands open their own.

The daemon does not listen for arbitrary clients. The app-side controller owns
the listening socket and accepts only a peer whose Unix socket credentials have
`uid=0`; non-root peers are closed and the controller keeps waiting within the
startup timeout. The daemon connects to that single controller.

## App-UID Session Start

Shizuku mode launches the same binary from the app process with `ProcessBuilder`,
so the child inherits the app UID and is an ordinary child of the app. Only the
APK library lookup, the linker invocation and the ABI check are shared with the
root path: there is no root command in between, and the app keeps the `Process`
handle it needs to read the child's `pid`, signal it and observe its exit.

The child normalizes one thing about how it was launched, before it builds its
runtime. A process inherits its scheduling policy from the thread that forked it,
and every thread it creates inherits that thread's, so the daemon inherits
whatever the app's coroutine dispatcher thread was running under - observed on
device as `SCHED_BATCH`, the fair class with wakeup preemption switched off. On
the app-UID path only, and only from `SCHED_BATCH` or `SCHED_IDLE`, the child
calls `sched_setscheduler(0, …)` for the ordinary fair policy on its main thread
before Tokio creates its workers, so the threads that run the dataplane inherit
it. Nothing else moves: nice value, scheduler cgroup and cpuset are Android's
placement of the app and are left exactly as they arrived, and a real-time policy
is never rewritten. Leaving `SCHED_IDLE` is a move up a scheduling class and the
kernel may refuse it for an unprivileged caller; `SCHED_BATCH`, the case actually
observed, has no such condition. The change is process-local, dies with the
process, and needs no cleanup path.

**Failures here are startup output, not structured reports.** This runs before
the runtime, the control socket and the conversation's reporter exist, so there
is no call to attach a report to; the refusal is written to the child's own
output, which the app drains into its log, and the session starts anyway.

**Root is deliberately untouched.** Its daemon is not forked from an app thread
at all: `RunDaemon.execute` runs inside the root service process and forks it
there, so whatever policy it inherits comes from that process rather than from
this app's dispatchers. Nothing about root's launch is changed, measured or
asserted here.

There is no separate handshake protocol. The connection carries the same
`ClientEnvelope`/`DaemonEnvelope` conversation root uses, with an app-UID command
family of its own - `StartShizukuSessionCommand` and `ApplyShizukuConfigCommand`.
Neither daemon serves the other's family: each refuses it as an error on the
offending call. Before writing anything, the app accepts only a peer whose Unix
credentials report this app's own `uid` and the exact `pid` of the process this
launch created. A rejected peer is discarded, and shutdown always keeps naming the
launched `pid`.

`StartShizukuSessionCommand` is an event-style call whose call ID owns the session
for as long as it runs:

1. the app writes it with the interface name and MTU, carrying exactly one
   `SCM_RIGHTS` descriptor: a duplicate of the TUN. The daemon's descriptor is
   independent of the app's from that moment, so closing the app's copy does not
   close the daemon's. Only the child dropping it does that, at session teardown
   or on exit: control-socket EOF makes a healthy child tear down and exit, while
   a wedged one can keep both itself and the descriptor. The descriptor rides on
   that one frame, so the app writes it with a single `write` on the socket and
   the daemon reads the whole frame with `recvmsg`;
2. the daemon splits the stream and starts its control writer *before* it looks at
   what the call says, and that writer is what makes everything after it
   answerable: a terminal `ErrorFrame` travels on it, not through the reporter. It
   then reads the call ID and the command, and installs the conversation's one
   nonfatal reporter once that call ID has been accepted - a nonfatal needs a
   conversation to belong to, and nothing before that point has one. It then
   re-checks the descriptor against the call: exactly one descriptor arrived, it is
   nonblocking, `TUNGETIFF` reports the expected interface, the flags include
   `IFF_TUN | IFF_NO_PI`, and `SIOCGIFMTU` reports the declared MTU. The sender
   cannot prove what arrived, and the MTU is immutable for the session, so both are
   read from the interface. A refusal closes every descriptor that arrived rather
   than keeping any;
3. the daemon registers the TUN with the reactor, reads the session's TCP seed,
   measures the descriptor budget, and reserves *and builds* every dataplane owner
   the session needs - the fixed byte reservations, the writer's channels, the
   reassembly table, and the UDP, Echo, virtual-DNS and TCP owners, each of which
   the admission budget can refuse. A refusal here releases what had already been
   reserved through the same fence a running session ends by;
4. it starts the ingress and egress tasks over what step 3 built, and only then
   sends the event ACK, which is the readiness the app returns on. Neither task
   constructs anything, so a task that exists at all owns a dataplane that fits.

Anything that fails in steps 2 to 4 is answered as an `ErrorFrame` on the start
call, carrying the structured report with its errno and Rust source location, so a
start that failed says why rather than closing the socket under the app. Two
failures cannot be answered and are not disguised as anything else: a frame
malformed before any call ID could be read, and a control writer that can no
longer carry a frame at all.

Observed child exit is raced only while no authenticated peer exists - around the
app's `accept` - so a child that dies in the linker or before it ever connects
fails the start at once with its exit status and the output it printed, rather
than leaving the start waiting on a connection nothing will ever make. Once the
exact `uid`/`pid` peer is accepted and the start call is written, that
conversation is authoritative: the app reads whatever the daemon already enqueued
before it believes the socket's EOF, and falls back to the process's exit status
only when the stream ended with no frame to attribute.

Each configuration is an ordinary one-shot call keyed to the start call's ID. The
newest configuration is the whole truth, and each is answered with a
`ShizukuApplied` reply only once whatever the change retires is really gone rather
than asked to go. Retiring a UDP mapping, for instance, cancels its receive task,
waits for that task to run to completion, drops the mapping's share of the socket,
and releases the descriptor's budget reservation only then. Platform resolver work
is the one exception, because it cannot be cancelled or joined; [`dns.md`](dns.md)
owns what it still holds. A configuration the daemon refuses is answered with an
`ErrorFrame` on its own call and ends the session, and the start call then gets no
second frame carrying the same failure. That `ErrorFrame` is described where the
refusal happened and written where the session ends, not at the refusal: like every
other terminal frame it is the session's last frame, so it follows the dataplane
join and the reporter's flush. A `CancelCommand` naming a call that is no longer
active asks for nothing: that is what an app whose caller was cancelled
mid-configuration leaves behind, and the conversation is serial, so the daemon
reads such a cancel only after it has already replied to the configuration it
names.

The start call's terminal frame is whichever of these the ending is: an
`ErrorFrame` with the one failure that ended the session, a `CompleteFrame` when
the dataplane finished cleanly with the app still connected, or nothing at all
when the app closed the control socket or cancelled the call.

Exactly one call is owed a terminal frame, and every terminal frame is the last
daemon-to-app frame the session writes; the control stream's EOF follows it. The
app reads this conversation with a single reader that returns on the first terminal
frame it sees - a terminal frame *is* how the session ends, whichever call it
names - so a frame written before the session's last reports would lose them. The
daemon therefore cancels and joins the dataplane, routes every failure the join
found, finishes the reporter, and only then enqueues the one frame it owes: the
start call's, or the `ErrorFrame` answering a refused configuration. It drops its
last writer sender afterwards, which is what ends the stream. No frame is enqueued
after the terminal frame, and a report raised after the reporter has finished goes
to stderr rather than onto the queue behind it.

One failure is delivered exactly once, and the session routes each to one of three
destinations: the terminal frame this session owes when nothing has claimed it yet;
no second delivery at all when the failure is the one that frame already carries,
which is a refused configuration; a structured nonfatal otherwise. No owner emits
its own report - the TUN writer and the session seed attach one to the error they
return and emit nothing - so a fatal egress failure or an entropy refusal reaches
the app as the start call's error, not as that error *and* a nonfatal saying the
same thing. The report delivered is the one the failing site built, so its errno,
details and Rust source location are that site's rather than the teardown's.

Shutdown waits for the child to be gone, because everything downstream assumes
it is: close the control socket, wait 10 seconds for exit, `destroy()` for
SIGTERM, wait 5 more, then SIGKILL the launched child PID and wait up to 5
seconds for observed exit, failing if exit is never observed; then cancel and
join the scope that reads the child's output and control stream.

Closing the socket is the whole of the *request*, since EOF on it is what the
daemon reads as cancellation and what makes it cancel and join its dataplane,
deliver what it still owed and close its copy of the TUN before returning from
`main`. No signal can ask for that - the daemon installs no handler, since its
Tokio build does not include the `signal` feature, so anything delivered to it
terminates it outright - which is why the escalation runs only after the graceful
window and never instead of it. The windows are the cleanup budget of a process
the app launched and still owns, not a deadline on any call: calls end on their
result or on the owner's cancellation and never on elapsed time. The policy is
the one `RootManager` already applies to the root process librootkotlinx starts
for it - a different process, reaped by a different owner, given the same
windows. The escalation exists because this teardown is non-cancellable, so a
child that will not act on EOF cannot be given up on by a user stop, and waiting
on it forever would leave it holding a duplicate of the TUN while the withdrawal,
and the next start joining it, stayed fenced with nothing left to recover them.
The final wait bounds an assertion rather than cooperation: nothing but an
uninterruptible sleep outlives SIGKILL, so exit that is still unobserved
afterwards fails the fence and retains the session instead of reporting it gone.

`destroyForcibly()` is not an escalation on Android: it calls `destroy()`, whose
native side is already `kill(pid, SIGTERM)`, so SIGKILL is sent explicitly, and
`ESRCH` from it is the child exiting between the check and the signal. The PID
comes from the process the app started, never from the connection, so a child
that never connects is still signalable, and it is also what the accepted
socket's peer credentials are checked against.

## Selected-Network Handover

A VPN reconnect or a default-network change moves the `Network` the daemon's
egress sockets are bound to. The app observes its own default network - per
calling UID, so a VPN when Android applies one to this app, and never the
root-only upstream preferences - rejects the session's own TUN by interface name,
increments `ShizukuSessionConfig.upstream_generation`, and sends the new handle in
the next configuration.

The daemon does not let the old work finish, since that would keep client traffic
egressing over the network the session is leaving for the full idle timeouts. Only
work bound to a `Network` is affected: TCP flows, UDP mappings and Echo sessions
are retired, while reassembly, the TUN writer, the virtual-DNS transports and the
resource accounting carry on. The order is:

1. publish the configuration's generation to the TUN writer and wait for the
   writer to confirm, so nothing produced under the superseded generation can
   still reach a client and the notifications below are not discarded with them;
2. cancel every affected flow, mapping and socket, then join their tasks and
   return what they held, so an acknowledged configuration means those descriptors
   are closed. An upstream connect still in flight is cancelled rather than waited
   for, and every wait inside a worker can be interrupted by its own cancellation,
   so a stalled peer cannot delay the acknowledgement;
3. close each retired upstream socket abortively, with `SO_LINGER` zero, so nothing
   further is transmitted over the network being left; UDP and ping sockets close
   directly;
4. notify each affected flow or query once: a reset per TCP flow that has a
   remote endpoint, and SERVFAIL for a resolver answer belonging to the
   superseded generation. UDP mappings and Echo sessions produce nothing,
   because a connectionless mapping has no ending to signal.

A submitted resolver transaction is never cancelled by a handover, because
cancelling would return this process's descriptor and nothing of the platform's
work. A DNS-over-TCP transport is unaffected because it terminates locally and
owns no socket bound to the upstream at all, and each of its
queries is fixed to the `Network` current when it was accepted, so an answer from a
selection the session has left becomes that query's own SERVFAIL while the
connection carries on ([`dns.md`](dns.md)).

Losing `ACTIVE` retires nothing. It clears `admit`, and closed admission means the
daemon drops every packet it reads from the TUN, creates nothing and refreshes no
lifetime; what already exists is left to end on its own deadlines and its own
protocols, both of which keep running throughout. Closed is therefore not paused,
and regaining `ACTIVE` resumes with whatever independently survived the interval
rather than with a guaranteed set of connections - a short interval normally
leaves the flows, mappings, Echo sessions, reassembly contexts and DNS transports
that were open, a long one leaves fewer. What it never does is retire anything
*for* the transition. There is no downstream retirement stamp to move: Android's
conntrack owns the mapping between a tethered client and the TUN-visible endpoint
the daemon keys by, so neither side can observe that such an endpoint changed
hands, and manufacturing a retirement for an `ACTIVE` transition would drop live
connections for a change nothing observed. Downstream membership does not reach
this decision at all, since the app watches tethering's upstream and never its
interface lists. `upstream_generation` may not move backwards; a configuration
that does is a protocol error and ends the session. Having no selectable `Network`
is not a failure: the configuration carries no handle, upstream work fails per
operation, and the session resumes on the next selection.

An update that cannot be carried or confirmed - a mid-frame write failure, a
refusal, or a missing or mismatched reply - ends the session, because the app can
no longer tell what the child is bound to. Stop and reapply is the way back. A
refused configuration reaches the app as that call's own structured error and is
reported as itself rather than as a generic message.

Cancellation of a configuration call is split at the write. A `CancelCommand` is
sent only when the whole command frame went out and the caller was then cancelled
awaiting its answer, which is the case the root controller cancels a call in.
Cancellation *inside* the write leaves a stream with no frame boundary left to
resynchronize on; appending a cancel there would write into the middle of a
configuration, so it is not attempted and the failure is terminal for the
connection.

Control-socket EOF ends the session too, and retires everything rather than only
what a configuration changed: the ingress task cancels and joins every owner
however its loop ended, and the session then joins both dataplane tasks, finishes
the reporter, answers whichever call is still owed a terminal frame - the start
call, or the configuration call a refusal named, and none at all after EOF - drops
its last writer sender, and joins the control writer, in that order. The session
also watches those two tasks directly, so a dataplane half that died ends it at once
instead of waiting for the app to speak.

## Calls

The control loop decodes one client envelope at a time and dispatches each
non-cancel command into a task tracked by call ID. `CancelCommand` is handled
before dispatch and cancels the active call's cancellation token.

There are two call shapes:

- one-shot calls return one reply or one terminal error;
- event-style calls keep the call ID active, deliver one or more event frames,
  and finish only when the controller cancels them, the daemon reports an error,
  the control connection closes, or the daemon sends a completion.

`StartSessionCommand` and `StartNeighbourMonitorCommand` are event calls.
`ReplaceSessionCommand`, `ReadTrafficCountersCommand`,
`ReplaceStaticAddressesCommand`, and `CleanRoutingCommand` are one-shot calls. The
app-UID path uses the same two shapes with its own command family - see
[App-UID Session Start](#app-uid-session-start) - and neither daemon serves the
other's family.

`StartSessionCommand` sends an event ACK after the session is established, then
keeps the call active as the session owner. The session event stream may later
carry optional daemon-to-routing requests, such as an IPsec forwarding-policy
update request. `StartNeighbourMonitorCommand` sends an initial
neighbour/topology snapshot and then streams updates. The protobuf schema still
describes the frames; "event-style call" only describes the controller lifetime
shape.

Call IDs are part of lifecycle ownership. A session is stored under the call ID
that started it. Closing that event call is the normal request to stop that
session.

In these docs, a session means the daemon-owned runtime for one downstream
interface named by `SessionConfig.downstream`. It bundles that interface's DNS
proxy listeners, optional NAT66 proxy state, and routing mutations. It is not a
client connection and it is not an upstream network.

## Session Startup

`StartSessionCommand` reserves a session slot before doing setup. The daemon
rejects a second active session for the same downstream interface. If an existing
session for that downstream has already been cancelled and is tearing down, the
new start waits for the old session to finish teardown and remove its daemon
slot, then retries insertion. If the new start is cancelled while waiting, it
exits without starting a session. If IPv6 NAT is requested, the process-wide
IPv6 NAT firewall base chains are attempted before the session runtime starts.
Failure there is reported as a structured nonfatal tied to the start call and
IPv6 NAT is disabled for that session start.

[`Session::start`](../../mobile/src/main/rust/vpnhotspotd/src/root/session.rs)
constructs the session in this order:

1. Open a temporary link/IPv4 event connection and the request connection that
   will ultimately belong to routing, then wait for a downstream IPv4 address.
2. Start the DNS runtime bound to that downstream IPv4 address. TCP and UDP
   listener setup is staged per MAC and protocol as independent best-effort
   capabilities.
3. Start NAT66 if requested. NAT66 TCP and UDP listener setup is staged per MAC
   and protocol; RA and ICMP setup remain session-level capabilities. The RA
   task owns separate IPv6-address event and request connections. Failures are
   reported as structured nonfatals tied to the start call. If the initial client
   set is empty, NAT66 is deferred with no interception. If clients are present
   and NAT66 produces no commit-ready TCP or UDP listener, the session continues
   with IPv6 NAT disabled.
4. Transfer the startup request connection into routing beside its applied
   mutation ledger and reconcile the staged DNS and NAT66 capabilities. Routing
   applies each mutation best effort and reports setup failures without rolling
   back unrelated successful mutations.
5. Publish only capabilities whose daemon resource and routing rule both
   committed. Staged per-MAC resources whose routing rules failed are cancelled
   before the ACK. If clients are present and routing commits no NAT66 TCP or
   UDP capability, NAT66 is stopped and the session is published with IPv6 NAT
   disabled.

Downstream IPv4 discovery is still required before a session can be established.
After that point, DNS, NAT66, and routing setup failures remove only the
affected MAC/protocol capability or mutation from the best-effort setup result.

After the session is installed, Rust publishes a session-control handle and
sends an event ACK. Read, replace, and stop operations enqueue commands through
that handle; the start-session task owns the session runtime and processes those
commands in order. When cancelled normally, it removes the control handle from
the slot, drains already queued commands, stops the session runtimes, removes the
session from daemon state, and releases any same-downstream start waiting behind
that teardown.

After the ACK, the daemon updates process-wide IPsec tracking for each active
session's upstream interface names and upstream generation. The tracked upstream
set is the union of primary and fallback upstream interfaces because either role
can be used by the installed routing policy for a given packet. On Android 12+,
a change to a session's upstream set or upstream generation spawns a best-effort
global probe that runs `/system/bin/dumpsys ipsec`, and its durable semantics
are:

- the probe is process-owned, never detached. The control loop cancels and joins
  every outstanding probe before it stops session state, flushes its
  process-global reporter or drops the sender that ends the control writer, so a
  probe cannot publish a policy for a torn-down session or lose its failure into
  a finished conversation. A stopping process admits no new probe;
- an upstream addition or replacement arriving during a probe is replayed as
  exactly one trailing rescan by the same flight, since that probe may have read
  the kernel before the new interface existed. A departure is not replayed: it
  drops refcounts only, and the probe in flight still speaks for the sessions
  that remain;
- results are committed only if the tracker's generation still matches. An
  upstream addition or replacement and a wholesale tracker clear both advance it,
  so an answer either of those made stale commits nothing and cannot resend
  targets the app already has. A session departure does not advance it: it drops
  refcounts, and its targets are filtered out of the result instead. Process
  shutdown needs no generation check at all, since it cancels and joins every
  probe before it clears tracking;
- the probe parses every matching IPv4 tunnel forwarding-policy target and the
  tracker emits only newly observed targets whose interface is still in an active
  session's upstream set. No-match is quiet; `dumpsys` or parser failures are
  structured global nonfatals. The emitted-target record is cleared when a target
  disappears from a later probe or its interface leaves all upstream sets;
- the daemon does not supervise a stuck `dumpsys`, and it never tracks or cleans
  up IPsec policy state: tunnel and policy teardown remain platform-owned.

## Session Replacement

`ReplaceSessionCommand` updates the config for an existing session. The
downstream interface is immutable; replacing it is rejected because routing and
session ownership are keyed to that interface. Replacement is ordered through
the session-control command loop, so it cannot interleave with traffic-counter
reads or session stop. It reuses routing's session-owned request connection.

Inside that ordered replacement, the session holds its config mutex as a commit
gate for DNS and NAT66 readers:

- routing reconciles from the previous committed config/capability set to the
  next desired config/capability set;
- NAT66 records replacement state that matters for later cleanup;
- the shared config snapshot is replaced;
- NAT66 is notified after the mutex is released.

This means active DNS/NAT66 work that needs a config snapshot can pause behind
replacement, but it cannot observe the next config before routing has committed
the matching interception state.

Client changes are MAC-scoped. Replacement stages DNS and NAT66 resources for
new MAC/protocol capabilities, reconciles routing, publishes only committed
capabilities, and cancels removed or uncommitted per-MAC resources. Before a MAC
or counter source is removed, the session exposes its final daemon-owned
counters through the next traffic-counter read.

When the next client set is empty, replacement publishes no NAT66 routing
capabilities. Existing NAT66 runtime state may stay alive only to preserve
session-owned counters and deferred NAT66 eligibility for a later non-empty
client set.

If process-wide firewall-base setup failed, NAT66 produced no runtime for a
non-empty client set, or routing committed no NAT66 TCP/UDP capability for a
non-empty client set, later replacements keep `ipv6_nat` disabled for that
session. An empty client set is not failure; replacement may start NAT66 when a
later neighbour snapshot adds a MAC.

After a successful replacement, the same process-wide IPsec state is updated
from the session's new upstream interface union and upstream generation. A
replacement triggers one IPsec probe when that interface union changes or when
Kotlin reports that either upstream role has a new upstream snapshot; if a
global probe is already running, the change is recorded and replayed as that
flight's rescan instead of starting a second one. Client and downstream-only
changes do not trigger a probe by themselves, and neither does a session losing
its last upstream.

## Shutdown And Clean

Normal session stop cancels the session stop token first so DNS and NAT66
listeners normally choose shutdown over reporting teardown-time socket errors.
It rolls routing back through the session-owned request connection without
waiting for listener or per-packet tasks to drain, then stops NAT66, which may
withdraw advertised prefixes through the same connection. Request failures are
reported without replacing the connection. NAT66 UDP associations and
per-listener DNS anchors release their reply-socket leases during this teardown.
An overlapping detached DNS task retains its own lease, so session stop or
replacement cannot close its reply socket prematurely. The process-wide
registry removes and closes an exact source bind when its last lease is
released.

When the control connection closes, the daemon cancels active calls, waits for
call tasks, stops the neighbour monitor, stops all sessions without extra
withdraw-cleanup, clears the IPsec aggregate, removes process-wide IPv6 NAT
firewall base state, drops the writer, and exits. Exiting also drops the NAT66
UDP reply-socket registry and closes any remaining file descriptors; the
registry creates no persistent routing or firewall state for Clean to remove.

`CleanRoutingCommand` is stronger than normal shutdown. It:

- drains all session slots from daemon state;
- marks those sessions as cleaning so their start-session tasks do not try to
  remove the same state again;
- detaches and completes the corresponding event calls;
- stops sessions with `withdraw_cleanup = true`;
- clears the process-wide IPsec upstream aggregate;
- removes the process-wide NAT66 firewall base chains;
- opens a scoped request connection and runs deterministic routing cleanup that
  reconstructs app-owned state from current kernel/interface state and the
  prefix seed.

Clean must not depend on private app databases, preferences, or daemon memory.
Anything that can outlive the process needs a deterministic cleanup path.
Traffic history is not a cleanup input. Per-MAC listeners, redirect rules,
TPROXY rules, and the single NAT66 ICMPv6 NFQUEUE rule are removed through
normal routing cleanup or deterministic Clean reconstruction.

## Neighbour Monitor

The daemon allows one neighbour monitor at a time. Starting a monitor opens a
multicast-only neighbour/link event connection plus a separate request
connection, sends an initial neighbour dump and bridge topology snapshot, then
streams deltas until cancellation. Stopping drops both connections. Link events
refresh bridge topology only when it changes.
