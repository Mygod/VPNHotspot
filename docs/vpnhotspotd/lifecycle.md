# Lifecycle

`vpnhotspotd` is started lazily by
[`DaemonController`](../../mobile/src/main/java/be/mygod/vpnhotspot/root/daemon/DaemonController.kt)
when the app sends the first daemon command. The daemon stays alive only while
the controller has active calls. When the last call is closed, Kotlin closes the
control connection; the Rust control loop then stops all daemon-owned runtime
state and exits.

## Entry Points

The binary has two entry contracts, selected by argument count in
[`main.rs`](../../mobile/src/main/rust/vpnhotspotd/src/main.rs):

- one argument, the control socket name: the root-side control loop described in
  the rest of this document;
- two arguments, the socket name and a bootstrap nonce: the Shizuku-mode app-UID
  path in [`bootstrap.rs`](../../mobile/src/main/rust/vpnhotspotd/src/bootstrap.rs),
  described under [App-UID Bootstrap](#app-uid-bootstrap).

Nothing the root control loop does is permitted at the app UID - every routing,
firewall, `ndc`, and NFQUEUE mutation needs root - so the two paths share the
process launcher, the frame format, and little else. Routing, `ndc` and NFQUEUE
mutation exist only on the root path. Sessions do not: the app-UID path has its own
session, built from the bootstrap handshake rather than from a `StartSessionCommand`,
and it carries the relay's own DNS and NAT66 for the TUN it was handed. What is
root-only there is the *root* session vocabulary - the call/reply envelope, the
session commands and the routing they configure - not the idea of a session.

## Process Startup

Kotlin locates the native `vpnhotspotd` library in the APK and runs it through
Android's linker from a root command. It creates:

- an abstract Unix-domain server socket name for the control channel;
- stdout and stderr pipes that are drained into Timber;
- one root command invocation that starts the daemon with the socket name.

The Rust entry point accepts exactly one argument: that socket name. It connects
back to the abstract Unix socket, splits the stream, starts a writer task for
outbound frames, installs the nonfatal reporter it owns for the rest of the
conversation - a second installation is refused, and a failed one is cleaned up by
cancelling and joining the writer that already exists - and builds process-wide
bookkeeping:

- one NAT66 ICMP dispatcher shared by NAT66 sessions and bound to the
  app-owned NFQUEUE number `30000`;
- one NAT66 UDP reply-socket registry shared by all MAC listeners and sessions,
  using the process's immutable daemon reply mark;
- a session map keyed by the start-session call ID;
- one process-wide upstream-interface aggregate for optional IPsec probes, beside the
  owner that holds those probes' task handles;
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
so the child inherits the app UID and is an ordinary child of the app rather than
of a root shell. The exec mechanism is the one above, unchanged: the linker plus
the in-place APK library path, and the same ABI check.

The handshake is three frames, and the daemon speaks first, because the app has to
authenticate the peer before handing over a descriptor:

1. the daemon sends `BootstrapHello` with the nonce it was launched with. The app
   requires peer `uid` equal to its own and the exact nonce. The nonce is what
   identifies the launched child specifically, since it travels in argv, which no
   other app can read. There is no protocol version, because the daemon is exec'd in
   place from the same APK as the dex that launched it and the two cannot disagree;
2. the app sends `BootstrapConfig` with the interface name and MTU, carrying
   exactly one `SCM_RIGHTS` descriptor: a duplicate of the TUN. What crosses is a
   reference to the same open file description, and the daemon's descriptor is
   independent of the app's from that moment: closing the app's copy does not close
   the daemon's, and retaining it does not keep the daemon's alive. The daemon's
   copy goes when the daemon drops it - at control EOF, at session teardown, or
   when the child dies - and it is the child dying, rather than any descriptor
   bookkeeping in the app, that ends a session the app process did not outlive;
3. the daemon replies `BootstrapReady` after re-checking the descriptor against
   the config: exactly one descriptor arrived, it is nonblocking, `TUNGETIFF`
   reports the expected interface, the flags include `IFF_TUN | IFF_NO_PI`, and
   `SIOCGIFMTU` reports the declared MTU. The MTU is immutable for the session and
   every packetization decision depends on it, so it is read from the interface
   rather than believed. The sender cannot prove what arrived, so none of this is
   taken on trust.

Two wire details are load-bearing and easy to get wrong. Ancillary descriptors are
state of `LocalSocketImpl`'s output stream, so the app writes that one frame with a
single `write` on the socket itself: a length prefix written byte by byte would
attach the descriptor once per byte, and a channel that writes the raw descriptor
by another route drops it entirely. On the daemon side the whole frame is read
through `recvmsg`, because a plain `read` that consumes the bytes the descriptor is
attached to discards it.

After the handshake the bootstrap hands off, and the session that follows is a
level-triggered config loop rather than the call/reply conversation the root mode
uses. Each config is answered with what was actually applied, and the answer waits on
the ingress task confirming that the state *the changed axes require to retire* is gone -
not merely that it was asked to go. Concretely, retiring a UDP mapping cancels its receive
task, waits for that task to run to completion, drops the mapping's own share of the
socket, and refunds the descriptor's budget charge only then. A message from the task
would not mean that, since it would be sent while the task still held the descriptor.
Nothing else can honestly report it either, because the session loop owns no dataplane
state.

What an acknowledgement deliberately does *not* cover is a resolver transaction. Those are
never cancelled to free capacity, because the platform owns the query once submitted, so one
outlives the config that retired whatever asked for it. What it keeps depends on whether this
process can still observe it. A transaction that stays observable holds its wrapper descriptor
and its logical token to completion, then discards or SERVFAILs the result. One whose wrapper
or readiness registration has become unobservable is different: this process closes or cancels
that descriptor and refunds the query's ordinary record and bytes, and only the *logical token*
standing for the platform's slot is quarantined until the session ends - the case spelled out
under [Selected-Network Handover](#selected-network-handover). Either way an acknowledged
config means the retired axes' own workers are joined and refunded, not that the daemon holds
nothing from before it.

### Selected-Network Handover

A VPN reconnect or a default-network handover changes which `Network` the daemon binds
to. It is not a transaction and acquires nothing: the app observes its own default
network - per calling UID, so a VPN when Android applies one to this app and the ordinary
default when it does not, and never the root-only upstream preferences - rejects the
session's own TUN by interface name, increments
`ShizukuSessionConfig.upstream_generation`, and sends the new handle in the next config.
The daemon owns the change from there, and it is a sweep rather than a drain - the old
`Network` is usually still usable, so letting established work finish would keep client
TCP, UDP and DNS egressing outside the new one for the full flow timers.

Only the egress layer is network-scoped: TCP flows, UDP mappings and Echo sessions hold
sockets bound to a `Network`, so they are retired. Ingress reassembly contexts, the
common writer, the virtual-DNS client transports and the budgets survive untouched,
because none of them owns one. What the sweep does, in order:

1. **Retire the writer first, and wait for it to say so.** Every producer stamps its
   packets with the generation and epoch it produced them under, and the writer drops a
   dequeued packet whose stamp is not the current one. Retirement is a command with an
   answer rather than a value the writer reads: a stamp it merely observed would close the
   enqueue side and say nothing about the write already parked in `AsyncFd::writable`, which
   would put a retired packet on the wire after the session had been told otherwise. So the
   writer adopts the new stamp, abandons that parked write, and only then acknowledges —
   after which nothing of the retired stamp can reach a client, including whatever an
   old-generation task enqueues before the sweeps below join it. The terminal packets those
   sweeps write carry the new stamp and leave.
2. **Quiesce, then join.** Every flow, mapping and ping socket is cancelled, so an
   acknowledged generation means nothing of the old one will be produced. Each owner then
   waits for every task it started to run to completion, takes back the record it kept,
   and refunds only then, so an acknowledged generation also means those descriptors are
   closed rather than merely asked to close. A TCP upstream connect still in flight is
   cancelled with the flow rather than waited for, because an unanswered SYN is bounded
   only by the kernel's connect timeout - and for the same reason every write,
   half-close, read and event handover in a worker races its own cancellation, so a
   stalled peer or a full queue cannot delay the acknowledgement either.

   Traffic still queued toward the retiring owners is not drained: it belongs to retired
   state and the stamp gate discards it, and a worker parked on that queue wakes on its
   own token instead. The one thing not joined here is a resolver transaction, which the
   platform owns once submitted - see step 4. Nor is one *owned* by the transport that
   asked for it: DNS-over-TCP transactions are rows in the ingress owner's own table,
   which a retirement does not touch.
   A flow that simply *fell idle* is not this. It is retired by the same steps in the same
   order - discard the queued payload and end of stream for that exact identity, cancel that
   flow's token, drop the upstream write half, abort the client socket, poll so the reset is
   really built, join, refund - but only the flow's own token is cancelled. The engine-wide
   sweep token means "the network these are bound to is being left", which is what selects
   the abortive close in step 3, and an idle flow's upstream has no reason to ask for it: it
   closes the ordinary way, and the remote reads an end of stream rather than a reset. A
   generation change that overtakes such a flow before its worker has finished is that
   change's decision to make and does close it abortively, because by then the network really
   is being left. The floors themselves are in
   [Shizuku Mode](shizuku.md#outer-tcp-phases).

   Config beats expiry, because the ingress task's select is biased and a config is the arm
   before the deadlines. That ordering is what keeps the terminal reset legible: the successor
   stamp is adopted before the retirement writes anything, so the packet passes the writer's
   own dequeue gate instead of being purged as belonging to the retirement being swept.

   The two endings do not double up either way round, and the second order is the one that
   needed saying. A config that arrives with a deadline already due empties the table before
   the expiry looks at it, so there is nothing left to expire. An expiry already begun leaves a
   row that is cancelled but still held - so the config **skips initiating anything for it**,
   because walking over it again would abort a socket this daemon has already closed and count a
   second reset nothing sends.

   What the config then does with that row depends on whether it still has a worker. An *attached*
   flow's record only leaves once that worker has been joined, so the config waits for it: the
   descriptor it holds belongs to the generation being left, and an acknowledged config has to
   mean that descriptor is gone. A *detached* flow has no worker and no terminal coming, so the
   config settles it directly rather than waiting - see
   [Shizuku Mode](shizuku.md#a-flow-can-outlive-its-worker). Either way the wait, unlike the
   initiation, covers every targeted row that can still produce one.

   Neither walk allocates. Both step through the round-robin order the fair queue registers
   every admitted flow into, which already holds each live handle exactly once, rather than
   collecting what is due into a list no lease covers.

   `STOPPING` neither pauses a floor nor rearms one. Once the stop's `closeAdmission` has been
   acknowledged it admits no new flow and no new exchange, and it does not refresh a deadline
   from payload it is draining; before that acknowledgement - or before the child is fenced
   when it cannot be obtained - the previous admitting config is still the one in force. Either
   way the deadlines keep running and what they retire is still taken back - with a reset for a
   flow that has a remote endpoint, and silently for one that has none - still listening, or
   already closed.

   A flow whose worker already finished is settled directly by a retirement or a shutdown rather than
   waited for. Both workers return as soon as their own ordered work is done, so a clean terminal hands
   the flow on and leaves the client's teardown running; waiting for a second terminal that can never
   arrive would hang the acknowledgement. See
   [Shizuku Mode](shizuku.md#a-flow-can-outlive-its-worker).
3. **Sweep abortively.** A swept flow's upstream socket is closed with `SO_LINGER` zero,
   because an ordinary close keeps transmitting queued bytes, retransmissions and the
   FIN over the network the session is leaving. A failure to set it is reported and the
   socket closed anyway. UDP and ping sockets close directly.
4. **Discard, then signal.** Bytes read from a swept socket are dropped rather than
   delivered. Each swept state then writes at most one terminal packet toward its client: a
   reset per TCP flow that has a remote endpoint to send one to - one still listening, or one
   already closed, is aborted silently and counted as no reset - and a SERVFAIL for a resolver
   answer whose generation was swept.
   Connectionless UDP and Echo get nothing. A resolver answer discarded because the
   *epoch* advanced is silent instead, since the address it would go to may name a
   different device by then.

   No resolver transaction is cancelled by a handover, on either transport. Cancelling
   would return this process's descriptor and nothing of the resolver's work - the
   platform's own per-UID slot stays taken until its work finishes - and it would destroy
   the completion that makes the debt exact. A UDP DNS query holds no selected-network
   socket, so it simply runs on and its answer is discarded on arrival.

   One submission cannot be settled that way at all. `android_res_nsend` is irreversible,
   so once it succeeds Android is holding a per-UID slot; if this process's own wrapper
   around the descriptor then fails, or the readiness registration it is being watched with
   later goes away, that slot's end is something nothing here can observe. Either way the
   query's record and bytes are refunded as usual, while the *logical token* standing for that
   slot is moved into a session-owned quarantine and released only when the session ends -
   refunding it would admit a second query against a limiter with no room for one. Which grant
   that token is moved *from* is per protocol: a UDP query holds its own, while for
   DNS-over-TCP it is the transport's connection that holds it - or, if that transport has
   already closed, the debt it handed the token to on the way out. A DNS-over-TCP transport this
   happens to is reset, since it cannot ask again under a token that no longer exists. A
   submission the platform *refused* is a different outcome and keeps none of this: nothing
   of Android's is held for it, so nothing has to be quarantined. What happens to the token
   then is the same per-protocol split: a UDP query owns its own, so a refusal really does
   return it with the rest of that query's grant, while a DNS-over-TCP query's debt owns no
   token at all in the ordinary case - the connection holds the one token and keeps it between
   questions - so the refusal returns the query's record and bytes and leaves the token exactly
   where it was, on a transport that is free to ask again.

   Neither is a DNS-over-TCP *transport* swept by a generation change, and for the same
   reason: it terminates locally and owns no socket bound to the network that changed.
   It keeps its socket, its mailbox, its logical resolver token and whatever question it
   has outstanding, while the ordinary flows beside it are reset, joined and refunded
   before the config is acknowledged. Which `Network` each of its queries went out on is
   fixed one query at a time, when the ingress owner accepts that query - so a request
   queued before the swap and accepted after it belongs to the successor, and an answer
   published under the predecessor is stale even if the successor kept the same handle.
   A stale-generation answer becomes that query's own SERVFAIL, delivered on the same
   connection under the current stamp, and the client asks again without reconnecting.

   An *epoch* change retires the transport with everything else, because its client tuple
   may name a different device now. Its transaction then runs on, because it is owned
   apart from the transport rather than as part of it: the transport is reset and joined,
   the transaction keeps its descriptor and takes the token with it, and the resolver slot
   is refunded when that transaction reaches its own terminal rather than when the flow
   did. That is a lifetime, not a task - the DNS-over-TCP transactions live in a prepared,
   charged table the ingress owner polls, so a transaction costs no spawn, no cancellation
   token and no per-query channel. That answer is discarded silently - it cannot reach the
   flow that reused the handle, and it refunds exactly once. The session's exit report is
   where an outstanding transaction shows up, and the only thing that cancels one is the
   session ending.

   A DNS-over-TCP transport does not need a selected network to *open*, either. It holds
   no socket bound to one, so a client's connection is accepted with none: its questions
   get their own SERVFAIL while there is nothing to resolve on, and the same stream
   resolves normally once a config supplies a network.

Losing `ACTIVE` advances `downstream_epoch` rather than the generation, and retires the
same state plus everything else keyed by a TUN-visible tuple, because Android may have
rebuilt the inner NAT behind an unchanged handle. Neither axis may move backwards; a
config that does is a protocol error and ends the session.

No selectable `Network` is not a failure. The config simply carries no handle, upstream
work fails per operation, and the session resumes on the next one. There is no fallback
network in this mode.

An update that cannot be carried or confirmed - a mid-frame write failure, a missing or
mismatched acknowledgement - ends the session explicitly. The app then cannot tell what
the child is bound to, retirement is daemon-side work it cannot ask for again, and
`DaemonIpc.writeFrame` writes the length and the payload separately, so there is nothing
to retry on. The ordered stop below is what fences the child; stop and reapply is the
way back.

Control-socket EOF is the authoritative cancellation signal, and on the daemon side it
retires everything rather than only what a config changed. The ingress task runs the same
quiesce-and-join for the UDP, Echo, TCP and DNS owners that a handover runs for one
stamp, whichever way its loop ended - EOF, cancellation, or its own failure - and only
then returns, so nothing daemon-owned is left for the runtime to abort. The session loop
joins the ingress and egress tasks, finishes the reporter, and joins the control writer,
in that order, so a report made by the last thing to fail still leaves - provided the writer
is still able to carry one. When the writer is what failed, it has already closed and drained
its queue, and those last reports go nowhere. What none of that settles is the platform's resolver work; process death does not
settle it either, and nothing here claims otherwise.

The session does not wait for the control socket to tell it that. It selects on the
ingress and egress tasks' own completion beside the socket, so a dataplane half that
failed, exited or did not complete at all ends the session at once: a quiet control socket
is ordinary - the app has nothing to say between configs - and a session that kept
acknowledging configs against tasks that were gone would be acknowledging nothing. The
first of those results is captured, the sibling and the conversation are cancelled, and
the same ordered teardown runs. It is the only teardown path: every failure after the
first task was spawned, including one during setup, arrives at it, and every handle it owns
is joined exactly once whether it completed on its own or is being stopped there. Errors
are folded rather than replaced, because a session that failed *and* could not shut down
cleanly is not the same as either alone.

On the app side, shutdown is a fence rather than a signal, because everything downstream
assumes the child is gone:
close the socket, wait 10 seconds for exit, `destroy()` for SIGTERM, wait 5 more,
then SIGKILL the launched child PID explicitly and wait for observed exit. That PID is
read from the process the app started, never from the connection: peer credentials
authenticate that the connected child *is* that process, and a child that never connects
has none at all, so taking the PID from the connection would leave that case unfenceable.
`destroyForcibly()` is not an escalation on Android - it calls `destroy()`, whose
native side is already `kill(pid, SIGTERM)`.

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
if a session's upstream set or upstream generation changes, the daemon spawns a
best-effort global probe that runs `/system/bin/dumpsys ipsec`. That probe is owned by the
process, not detached: its handle is retained, one token cancels every outstanding one, and
the control loop cancels and joins all of them before it stops session state, flushes its
process-global reporter or drops the sender that ends the control writer - a probe mutates
the shared tracker and sends frames, so a detached one could publish a policy for a session
that is already torn down or lose its own failure into a conversation that is over.

That is the orderly order. The other way out is the control writer failing: it closes and
drains its own queue, cancels the conversation and is then about to return, so the loop can
observe the cancellation before the task has finished - and it is the final join that carries
the error out. Cancellation is what drives the probe join and the session teardown that
follow, and reports raised during them have nowhere left to go, which is why a write failure
ends the conversation rather than being survived. A probe an already-stopping process refuses
to admit is not started at all. Cancellation is quiet; a probe that did not run to completion is
structured, and a completed one is joined at the next admission rather than accumulating a
handle for the life of the conversation.

An upstream *addition or replacement* that arrives while a probe is running is replayed rather
than dropped. That probe may have read the kernel's policies before the new interface existed,
so its answer cannot be taken for the new upstream: the tracker moves its generation, which
invalidates the answer in flight, records the update and hands it back when the probe
completes, and the same flight scans again. However many such updates pile up, exactly one
rescan follows, and the flight stays open in the meantime so nothing starts a second scan
beside it.

A *departure* is not one of those. A session removed, or one whose upstream set becomes empty,
only drops its refcounts and the emitted targets they held: the generation does not move, so
the probe in flight is neither invalidated nor replayed and still speaks for the sessions that
remain.

The probe parses every matching IPv4 tunnel forwarding-policy
target in the dump, and the process-wide tracker emits only newly observed
targets whose interface is still in an active session's upstream set. Repeated
probes that observe the same target do not emit another request. No-match is
quiet; `dumpsys` or parser failures are structured global nonfatals. The daemon
clears its emitted-target record when the target disappears from a later probe or
its interface leaves all session upstream sets.

Clearing the tracker - a clean, or the process stopping - moves a generation with it, and a
probe started for the sessions that were dropped commits nothing when it comes back. This is
the one place two probes can overlap, because clearing is also what lets the next update start
one, and committing the older answer would do more than go stale: the targets it did not see
include the ones the newer probe has already sent to the app, and forgetting those is exactly
what would send them again.

The daemon does not separately
supervise a stuck `dumpsys` process, and it does not track or clean up IPsec
policy state; tunnel and policy teardown remain platform-owned.

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
Kotlin reports that either upstream role has a new upstream snapshot; if a global
probe is already running, the change is recorded and replayed as that flight's
rescan instead of starting a second one. Client and downstream-only changes do not
trigger a probe by themselves, and neither does a session losing its last upstream.

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
