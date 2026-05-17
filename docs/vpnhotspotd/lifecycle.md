# Lifecycle

`vpnhotspotd` is started lazily by
[`DaemonController`](../../mobile/src/main/java/be/mygod/vpnhotspot/root/daemon/DaemonController.kt)
when the app sends the first daemon command. The daemon stays alive only while
the controller has active calls. When the last call is closed, Kotlin closes the
control connection; the Rust control loop then stops all daemon-owned runtime
state and exits.

## Process Startup

Kotlin locates the native `vpnhotspotd` library in the APK and runs it through
Android's linker from a root command. It creates:

- an abstract Unix-domain server socket name for the control channel;
- stdout and stderr pipes that are drained into Timber;
- one root command invocation that starts the daemon with the socket name.

The Rust entry point accepts exactly one argument: that socket name. It connects
back to the abstract Unix socket, splits the stream, starts a writer task for
outbound frames, initializes the nonfatal reporter, and builds process-wide
bookkeeping:

- one lazy rtnetlink runtime slot;
- one NAT66 ICMP dispatcher shared by NAT66 sessions;
- a session map keyed by the start-session call ID;
- one optional neighbour monitor;
- one process-wide flag for the NAT66 firewall base chains.

The rtnetlink runtime is created on the first command that needs netlink or
routing state: session start, neighbour monitoring, static-address replacement,
or Clean. Commands that do not need netlink, such as traffic-counter reads, do
not open the rtnetlink connection. Once created, the runtime remains
process-wide until daemon exit.

The daemon does not listen for arbitrary clients. The app-side controller owns
the listening socket and the daemon connects to that single controller.

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
keeps the call active as the session owner. `StartNeighbourMonitorCommand`
sends an initial neighbour/topology snapshot and then streams updates. The
protobuf schema still describes the frames; "event-style call" only describes
the controller lifetime shape.

Call IDs are part of lifecycle ownership. A session is stored under the call ID
that started it. Closing that event call is the normal request to stop that
session.

In these docs, a session means the daemon-owned runtime for one downstream
interface named by `SessionConfig.downstream`. It bundles that interface's DNS
proxy listeners, optional NAT66 proxy state, and routing mutations. It is not a
client connection and it is not an upstream network.

## Session Startup

`StartSessionCommand` reserves a session slot before doing setup. The daemon
rejects a second active session for the same downstream interface. If IPv6 NAT
is requested, the process-wide IPv6 NAT firewall base chains are attempted
before the session runtime starts. Failure there is reported as a structured
nonfatal tied to the start call and IPv6 NAT is disabled for that session start.

[`Session::start`](../../mobile/src/main/rust/vpnhotspotd/src/session.rs)
constructs the session in this order:

1. Ensure the process-wide rtnetlink runtime exists, then wait for a downstream
   IPv4 address through netlink notifications and the caller's cancellation
   token.
2. Start the DNS runtime bound to that downstream IPv4 address. TCP and UDP
   listener setup are independent best-effort capabilities.
3. Start NAT66 if requested. NAT66 TCP, UDP, RA, and ICMP setup failures are
   reported as structured nonfatals tied to the start call. If NAT66 produces
   no TCP or UDP listener, the session continues with IPv6 NAT disabled.
4. Start routing using the DNS ports and optional NAT66 capabilities produced
   by the runtimes. Routing applies each mutation best effort and reports setup
   failures without rolling back other successful mutations.

Downstream IPv4 discovery is still required before a session can be established.
After that point, DNS, NAT66, and routing setup failures remove only the
affected capability or mutation from the best-effort setup result.

After the session is installed, Rust sends an event ACK. The start-session task
then waits for cancellation. When cancelled normally, it removes the session and
stops the session runtimes.

## Session Replacement

`ReplaceSessionCommand` updates the config for an existing session. The
downstream interface is immutable; replacing it is rejected because routing and
session ownership are keyed to that interface.

Replacement happens under the session's config mutex:

- routing reconciles from the previous config to the next desired config;
- NAT66 records replacement state that matters for later cleanup;
- the shared config snapshot is replaced;
- NAT66 is notified after the mutex is released.

If NAT66 produced no runtime during startup or was disabled by process-wide
firewall-base setup failure, later replacements keep `ipv6_nat` disabled for
that session. A replacement does not retry NAT66 startup.

## Shutdown And Clean

Normal session stop removes routing first, cancels the session stop token, then
stops NAT66. NAT66 may withdraw router-advertised prefixes during stop.

When the control connection closes, the daemon cancels active calls, waits for
call tasks, stops the neighbour monitor, stops all sessions without extra
withdraw-cleanup, removes process-wide IPv6 NAT firewall base state, drops the
writer, and exits.

`CleanRoutingCommand` is stronger than normal shutdown. It:

- drains all session slots from daemon state;
- marks those sessions as cleaning so their start-session tasks do not try to
  remove the same state again;
- detaches and completes the corresponding event calls;
- stops sessions with `withdraw_cleanup = true`;
- removes the process-wide NAT66 firewall base chains;
- runs deterministic routing cleanup that reconstructs app-owned state from
  current kernel/interface state and the prefix seed.

Clean must not depend on private app databases, preferences, or daemon memory.
Anything that can outlive the process needs a deterministic cleanup path.

## Neighbour Monitor

The daemon allows one neighbour monitor at a time. Starting a monitor registers
single-consumer netlink neighbour and link event slots, sends an initial dump
with bridge topology, then streams deltas until the event call is cancelled.

Stopping the monitor drops both netlink registrations and waits for the monitor
task. Link events trigger bridge topology snapshots only when the topology
actually changes.
