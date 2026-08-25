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
and receives a configuration stream rather than calls: no call IDs, no session
commands, and no root mutations. Its session is built from the bootstrap
handshake below, and the feature it serves is described in
[`shizuku.md`](shizuku.md).

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

## App-UID Bootstrap

Shizuku mode launches the same binary from the app process with `ProcessBuilder`,
so the child inherits the app UID and is an ordinary child of the app. Only the
APK library lookup, the linker invocation and the ABI check are shared with the
root path: there is no root command in between, and the app keeps the `Process`
handle it needs to signal the child.

The handshake is two frames. Before sending either, the app accepts only a peer
whose Unix credentials report this app's own `uid` and the exact `pid` of the
process this launch created. A rejected peer is discarded, and shutdown always
keeps naming the launched `pid`.

1. the app sends `BootstrapConfig` with the interface name and MTU, carrying
   exactly one `SCM_RIGHTS` descriptor: a duplicate of the TUN. The daemon's
   descriptor is independent of the app's from that moment, so closing the app's
   copy does not close the daemon's. Only the child dropping it does that, at
   session teardown or on exit: control-socket EOF makes a healthy child tear
   down and exit, while a wedged one can keep both itself and the descriptor;
2. the daemon replies `BootstrapReady` after re-checking the descriptor against
   the config: exactly one descriptor arrived, it is nonblocking, `TUNGETIFF`
   reports the expected interface, the flags include `IFF_TUN | IFF_NO_PI`, and
   `SIOCGIFMTU` reports the declared MTU. The sender cannot prove what arrived, and
   the MTU is immutable for the session, so both are read from the interface.

The descriptor rides on that one frame, so the app writes it with a single `write`
on the socket and the daemon reads the whole frame with `recvmsg`.

After the handshake the child is driven by a configuration stream: the newest
configuration is the whole truth, and each is acknowledged only once whatever
the change retires is really gone rather than asked to go. Retiring a UDP
mapping, for instance, cancels its receive task, waits for that task to run to
completion, drops the mapping's share of the socket, and releases the
descriptor's budget reservation only then. Platform resolver work is the one
exception, because it cannot be cancelled or joined; [`dns.md`](dns.md) owns
what it still holds.

Shutdown waits for the child to be gone, because everything downstream assumes
it is: close the control socket, wait 10 seconds for exit, `destroy()` for
SIGTERM, wait 5 more, then SIGKILL the launched child PID and wait up to 5
seconds for observed exit, failing if exit is never observed; then cancel and
join the scope that reads the child's output and control stream. The PID comes
from the process the app started, never from the connection, so a child that
never connects is still signalable. `destroyForcibly()` is not an escalation on
Android: it calls `destroy()`, whose native side is already
`kill(pid, SIGTERM)`.

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

1. publish the configuration's generation and epoch to the TUN writer - a change
   advances whichever of the two it concerns - and wait for the writer to confirm,
   so nothing produced under the superseded values can still reach a client and
   the notifications below are not discarded with them;
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
   superseded generation. UDP mappings and Echo sessions produce nothing, and an
   answer dropped because the epoch advanced is silent, since the TUN-visible
   address it would go to may name a different device.

A submitted resolver transaction is never cancelled by a handover, because
cancelling would return this process's descriptor and nothing of the platform's
work. A DNS-over-TCP transport is unaffected because it terminates locally and
owns no socket bound to the upstream at all, and each of its
queries is fixed to the `Network` current when it was accepted, so an answer from a
selection the session has left becomes that query's own SERVFAIL while the
connection carries on. An epoch change does retire the transport, because the
client address may no longer name the same device, while its transaction runs on
([`dns.md`](dns.md)).

Losing `ACTIVE` advances `downstream_epoch` instead of the generation and retires
everything keyed to a TUN-visible address and port. Neither value may move
backwards; a configuration that does is a protocol error and ends the session.
Having no selectable `Network` is not a failure: the configuration carries no
handle, upstream work fails per operation, and the session resumes on the next
selection.

An update that cannot be carried or confirmed - a mid-frame write failure, or a
missing or mismatched acknowledgement - ends the session, because the app can no
longer tell what the child is bound to. Stop and reapply is the way back.

Control-socket EOF ends the session too, and retires everything rather than only
what a configuration changed: the ingress task cancels and joins every owner
however its loop ended, and the session then joins both dataplane tasks, finishes
the reporter and joins the control writer, in that order. The session also watches
those two tasks directly, so a dataplane half that died ends it at once instead of
waiting for the app to speak.

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
`ReplaceStaticAddressesCommand`, and `CleanRoutingCommand` are one-shot calls.

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

[`Session::start`](../../mobile/src/main/rust/vpnhotspotd/src/session.rs)
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
