# Lifecycle

The binary accepts a socket name for root mode, or `--app-uid` plus a socket
name for the Shizuku child. They share framing and launch support; root routing,
firewall, `ndc` and NFQUEUE state is never reachable from the app-UID path.

Both modes use pinned Tokio 1.53.1's multi-threaded runtime default: one worker
thread per value returned by `available_parallelism()`, as implemented by its
[`Builder`](https://github.com/tokio-rs/tokio/blob/tokio-1.53.1/tokio/src/runtime/builder.rs#L2004-L2022).
This bounds simultaneous task polling, not the number of admitted tasks. Runnable
tasks wait in Tokio's scheduler rather than being refused; failure to determine or
allocate the runtime's worker resources makes runtime construction fail before
root or app-UID startup.

## Root Process Startup

[`DaemonController`](../../mobile/src/main/java/be/mygod/vpnhotspot/root/daemon/DaemonController.kt)
starts the daemon lazily on the first command. Kotlin creates an abstract Unix
socket, drains stdout/stderr into Timber and runs the APK library through
Android's linker from a root command. It accepts only a peer with `uid=0`.

The daemon connects back, starts its control writer and nonfatal reporter, and
creates process-wide state:

- NAT66 ICMP dispatcher on NFQUEUE `30000`;
- NAT66 UDP reply-socket registry;
- session map keyed by start-call ID;
- upstream aggregate and owned IPsec probe;
- optional neighbour monitor;
- NAT66 firewall-base state.

Rtnetlink event and request connections have separate owners. Session routing
keeps its request connection for the session lifetime; one-shot commands open
their own.

When the last root call closes, Kotlin closes the control connection. The daemon
stops all owned work and exits.

## App-UID Session

[`AppUidDaemon`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/AppUidDaemon.kt)
launches the child with `ProcessBuilder`. The child inherits the app UID; the app
retains its `Process` and PID and accepts only a peer whose credentials match
both. On the app-UID path, inherited `SCHED_BATCH` or `SCHED_IDLE` is reset to
ordinary `SCHED_OTHER` before Tokio starts. Failing to read or reset the policy
is logged immediately, then reported once as a nonfatal after the session reporter
is installed. Startup continues under the inherited policy.

`StartShizukuSessionCommand` is the event call that owns the session. Each direct
`ShizukuSessionConfig` is a one-shot call correlated by its own `call_id`.
Admission and acknowledgement semantics are documented in
[`shizuku.md`](shizuku.md#configuration-and-admission).

Start refusal and session failure return an `ErrorFrame` on the start call.
Config refusal returns an `ErrorFrame` on that config call and ends the session.
The daemon joins the dataplane and flushes nonfatal reports before sending the
single terminal frame owed by the session; no frame follows it. Control EOF
requests graceful teardown and sends no terminal frame.

The child holds an independent duplicate of the TUN. Ordered stop closes the
control socket and escalates from an exit wait to SIGTERM and SIGKILL as described
in [`shizuku.md`](shizuku.md#ownership-and-lifecycle). Startup, system publication
and external cleanup are documented there rather than repeated here.

## Root Calls

The root control loop decodes one envelope at a time and tracks each dispatched
call by call ID. `CancelCommand` cancels the named active call.

- One-shot calls return one reply or terminal error.
- Event calls keep the ID active, stream events, and end on cancellation,
  failure, completion or control EOF.

`StartSessionCommand` and `StartNeighbourMonitorCommand` are event calls.
`ReplaceSessionCommand`, `ReadTrafficCountersCommand`,
`ReplaceStaticAddressesCommand`, and `CleanRoutingCommand` are one-shot calls.

The start-session call ID owns the root session and keys its slot. A root session
is the DNS, optional NAT66 and routing runtime for one immutable downstream
interface, not a client connection or upstream network.

## Root Session Startup

`StartSessionCommand` reserves the downstream slot. A same-downstream successor
waits for a cancelling predecessor to finish teardown; cancellation while waiting
starts nothing. If NAT66 firewall-base setup fails, the failure is a nonfatal and
that session starts without NAT66.

[`Session::start`](../../mobile/src/main/rust/vpnhotspotd/src/root/session.rs)
performs:

1. open link/address event and request connections and await downstream IPv4;
2. stage per-MAC TCP/UDP DNS listeners;
3. stage NAT66 per-MAC TCP/UDP listeners and session RA/ICMP capabilities;
4. transfer the request connection to routing and reconcile desired mutations;
5. publish only capabilities whose runtime and routing interception both
   committed, cancelling uncommitted staged resources before ACK.

An empty client set defers NAT66. Other DNS, NAT66 and routing failures remove
only the affected optional capability where possible. A start that fails cancels
its staged listeners, RA loop and netlink connections. Call cancellation is
observed while awaiting downstream IPv4; after discovery, startup produces an
owned `Session` and then runs its normal rollback instead of dropping partially
applied external state.

After ACK, the start task owns the session command queue. Replace, read and stop
run in order. On stop it removes the public handle, drains queued commands, stops
the runtime, removes the slot and wakes any same-downstream successor.

Android 12+ session changes may start one process-owned `dumpsys ipsec` probe.
The active probe coalesces additions/replacements into one trailing rescan;
departures only reduce the tracked set. Results commit only for the current
tracker generation and emit newly observed IPv4 forwarding-policy targets still
owned by an active upstream. No match is quiet; command/parser failure is a
global nonfatal. The control loop cancels and joins the probe before clearing
tracking or ending report delivery. VPNHotspot does not own or clean platform
IPsec policy state.

## Root Session Replacement

`ReplaceSessionCommand` rejects a changed downstream and runs in the session's
ordered command loop. The config mutex is the publication gate:

1. routing reconciles old to new desired state;
2. NAT66 records replacement state needed for cleanup;
3. the shared config snapshot is replaced;
4. NAT66 is notified after the lock is released.

This prevents DNS/NAT66 readers from observing new config before matching
interception has committed. Client changes stage and publish per-MAC capabilities
independently; removed or uncommitted resources are cancelled. Final counters are
made available before their source is removed.

An empty client set publishes no NAT66 interception but may retain runtime state
needed for counters and later reactivation. A session that failed NAT66 base,
runtime or all TCP/UDP interception for a non-empty client set keeps NAT66
disabled; an initially empty set may start it when clients appear.

Upstream changes update the process-wide IPsec tracker. Additions or replacements
arriving during a probe request its one trailing rescan. Client-only, downstream-
only and last-upstream removal changes do not start a probe.

## Root Shutdown And Clean

Normal stop cancels the session token, rolls routing back through the retained
request connection, then stops NAT66 and withdraws any advertised prefixes.
Failures are reported without replacing the connection. NAT66 associations and
DNS tasks release reply-socket leases; a detached DNS query may retain its own
lease until completion.

On control EOF, the daemon cancels and joins active calls and IPsec work, stops
the neighbour monitor and sessions, clears process-wide NAT66 firewall state,
releases its own control state, waits for every detached report-capable future,
finishes its nonfatal reporter, ends the writer and exits. Detached DNS, NAT66, RA
and ICMP tasks are tracked through cancellation; rtnetlink request drivers are
tracked until their owning connection drops. Their destructors are included.
Pending waits race stop tokens, and continuously ready drains recheck cancellation
per item. This keeps reporting open through teardown and runs resolver cancellation
before process exit. Exiting drops the reply-socket registry and remaining
descriptors. See [`errors.md`](errors.md#coalescing-and-delivery).

`CleanRoutingCommand` additionally:

- drains session slots and completes their event calls;
- stops sessions with withdrawal cleanup;
- clears IPsec tracking and NAT66 firewall-base chains;
- reconstructs deterministic cleanup from current kernel/interface state and
  the prefix seed through a scoped request connection.

Clean never depends on private app storage or daemon memory. The exact mutations
it reconstructs are listed in [`routing.md`](routing.md#clean-mutations).

## Neighbour Monitor

Only one monitor may run. It owns a multicast neighbour/link connection and a
separate request connection, sends the initial neighbour and bridge snapshot,
then streams deltas until cancellation. Link events refresh bridge topology only
when it changes.
