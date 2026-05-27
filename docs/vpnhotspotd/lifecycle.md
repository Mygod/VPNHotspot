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
- one NAT66 ICMP dispatcher shared by NAT66 sessions and bound to the
  app-owned NFQUEUE number `30000`;
- a session map keyed by the start-session call ID;
- one process-wide upstream-interface aggregate for optional IPsec probes;
- one optional neighbour monitor;
- one process-wide flag for the NAT66 firewall base chains.

The rtnetlink runtime is created on the first command that needs netlink or
routing state: session start, neighbour monitoring, static-address replacement,
or Clean. Commands that do not need netlink, such as traffic-counter reads, do
not open the rtnetlink connection. Once created, the runtime remains
process-wide until daemon exit.

The daemon does not listen for arbitrary clients. The app-side controller owns
the listening socket and accepts only a peer whose Unix socket credentials have
`uid=0`; non-root peers are closed and the controller keeps waiting within the
startup timeout. The daemon connects to that single controller.

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
   listener setup is staged per MAC and protocol as independent best-effort
   capabilities.
3. Start NAT66 if requested. NAT66 TCP and UDP listener setup is staged per MAC
   and protocol; RA and ICMP setup remain session-level capabilities. Failures
   are reported as structured nonfatals tied to the start call. If the initial
   client set is empty, NAT66 is deferred with no interception. If clients are
   present and NAT66 produces no commit-ready TCP or UDP listener, the session
   continues with IPv6 NAT disabled.
4. Start routing using the staged DNS and optional NAT66 capabilities produced
   by the runtimes. Routing applies each mutation best effort and reports setup
   failures without rolling back unrelated successful mutations.
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
the slot, drains already queued commands, removes the session from daemon state,
and stops the session runtimes.

After the ACK, the daemon updates a process-wide aggregate of upstream interface
names across all active sessions. On Android 12+, if an interface name enters
that aggregate set, the daemon spawns a best-effort probe that runs
`/system/bin/dumpsys ipsec`. When the probe completes, each matching IPv4
tunnel forwarding-policy request is emitted to one currently active session
call that still references the target's upstream interface. No-match is quiet;
`dumpsys` or parser failures are structured nonfatals tied to one currently
active session call for the probed interfaces, if any. The daemon does not
separately supervise a stuck `dumpsys` process, and it does not track or clean
up IPsec policy state; tunnel and policy teardown remain platform-owned.

## Session Replacement

`ReplaceSessionCommand` updates the config for an existing session. The
downstream interface is immutable; replacing it is rejected because routing and
session ownership are keyed to that interface. Replacement is ordered through
the session-control command loop, so it cannot interleave with traffic-counter
reads or session stop.

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

After a successful replacement, the same process-wide IPsec upstream aggregate
is updated from the session's new upstream interface set. A replacement only
triggers an IPsec probe for interface names that enter the aggregate set; client
changes, downstream changes, and primary/fallback role changes do not trigger a
probe by themselves.

## Shutdown And Clean

Normal session stop cancels the session stop token first so DNS and NAT66
listeners normally choose shutdown over reporting teardown-time socket errors.
Shutdown does not wait for listener or per-packet tasks to drain before removing
routing state. It then stops NAT66, which may withdraw router-advertised
prefixes during stop.

When the control connection closes, the daemon cancels active calls, waits for
call tasks, stops the neighbour monitor, stops all sessions without extra
withdraw-cleanup, clears the IPsec aggregate, removes process-wide IPv6 NAT
firewall base state, drops the writer, and exits.

`CleanRoutingCommand` is stronger than normal shutdown. It:

- drains all session slots from daemon state;
- marks those sessions as cleaning so their start-session tasks do not try to
  remove the same state again;
- detaches and completes the corresponding event calls;
- stops sessions with `withdraw_cleanup = true`;
- clears the process-wide IPsec upstream aggregate;
- removes the process-wide NAT66 firewall base chains;
- runs deterministic routing cleanup that reconstructs app-owned state from
  current kernel/interface state and the prefix seed.

Clean must not depend on private app databases, preferences, or daemon memory.
Anything that can outlive the process needs a deterministic cleanup path.
Traffic history is not a cleanup input. Per-MAC listeners, redirect rules,
TPROXY rules, and the single NAT66 ICMPv6 NFQUEUE rule are removed through
normal routing cleanup or deterministic Clean reconstruction.

## Neighbour Monitor

The daemon allows one neighbour monitor at a time. Starting a monitor registers
single-consumer netlink neighbour and link event slots, sends an initial dump
with bridge topology, then streams deltas until the event call is cancelled.
NUD stale entries are reported as cached instead of active, preserving the
kernel's MAC/IP cache for callers that can use it while letting UI counters and
timeout decisions ignore fully stale-only clients.

Stopping the monitor drops both netlink registrations and waits for the monitor
task. Link events trigger bridge topology snapshots only when the topology
actually changes.
