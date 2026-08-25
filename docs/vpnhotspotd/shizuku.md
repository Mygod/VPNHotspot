# Shizuku Mode

Shizuku mode shares this app's own default connection with tethered clients without root. It publishes a
restricted `TRANSPORT_TEST` network over a TUN it owns, lets Android's system tethering select that network
as its upstream, and relays the resulting traffic from an app-UID child process onto whatever `Network`
Android has made this app's default: a VPN when one applies to this UID, and the ordinary default when none
does.

Requires Android 13 (API 33) or later.

It is **one global upstream mode**. It owns no downstream: it never reads, stores or makes policy from a
tethered interface name, and it never starts, stops or cycles Android system tethering. Everything it
publishes is a property of the session's own TUN and of the exact `Network` the agent registered.

Root mode and this mode are independent in lifecycle, control-plane state and resource ownership. Both may
run at once, neither consults the other, and `RoutingManager`, `Routing` and `TetheringService` are
unchanged by this mode. The kernel alone arbitrates packets: root's per-interface policy rules sit at the
priorities in [`routing.md`](routing.md#priority-and-table-model), all numerically below the tethering
upstream rule Android installs for the selected network, so root's routing wins for the downstreams it
carries.

Shizuku's shell or root identity is used **only** for Android control operations. The dataplane child and
every egress socket run at the app UID.

## Scope And Limitations

Root mode has none of these, and remains the recommended path.

- **No isolation from other apps on the device.** Any app with network access that can name the interface
  can put packets on it. See [Security Boundary](#security-boundary).
- **No client identity.** Android's inner NAT translates client addresses, ports and Echo identifiers before
  packets reach the TUN, and any app with network access can forge a source. There is no per-client blocking
  and no per-client accounting; see [`traffic.md`](traffic.md).
- **Bounded traffic support.** TCP, UDP, DNS terminated at the virtual resolver addresses, ICMP Echo, and a
  bounded set of translated ICMP errors. ESP, GRE, SCTP, unknown IP protocols and downstream link control
  (RA/RS/ND, DHCP, ARP) are not carried; Android tethering owns downstream link control.
- **IPv6 reaches at most one downstream.** Tethering delegates an upstream `/64` only to the oldest active
  downstream
  ([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#201)),
  so a second tethered interface is IPv4-only, and a local-only downstream holding that position
  ([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#191))
  leaves every tethered interface without IPv6.
- **Selection is not commanded.** `setPreferTestNetworks` is a preference, not a reselection trigger; see
  [System Tethering Selection](#system-tethering-selection).
- **Losing the network does not stop tethering.** Android reselects an ordinary upstream and clients keep
  working, unprotected, with nothing on their side changing to say so.
- **No TestNetwork coexistence.** Startup refuses to run while any `TRANSPORT_TEST` network exists, and a
  session whose tethering upstream turns out to be a `TRANSPORT_TEST` network it does not own ends. Neither
  is recoverable by cycling tethering.

## Ownership And Lifecycle

| Owner | Owns |
| --- | --- |
| [`ShizukuTestNetwork`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/ShizukuTestNetwork.kt) | one session at a time: TUN descriptor, exact request, agent, pinned tethering connector, global preference, child, upstream observation, and the record of what each still owes |
| [`ShizukuLifecycle`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/ShizukuLifecycle.kt) | ordering only: the one job in flight, its finalizer, and the process-wide reference a successor waits on |
| [`ShizukuTetheringService`](../../mobile/src/main/java/be/mygod/vpnhotspot/ShizukuTetheringService.kt) | that job, the scope it runs on, and the process lifetime behind it |
| [`AppUidDaemon`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/AppUidDaemon.kt) | the launched child, its authentication, and the configuration stream |
| The daemon's ingress task | every TUN-visible flow, mapping and transport, and the tasks they start |

The lifecycle contract is:

- one session at a time, driven by one cancellable `Job` the foreground service owns;
- start and stop return immediately. Waiting for the previous cleanup and acquiring a new session are both
  cancellable, so there is no starting or stopping state to render;
- cleanup cannot be cancelled. Once entered - whether it is the inherited cleanup at the head of a start or
  the session's own at the end - it runs under `NonCancellable` until it completes or fails, and a step that
  fails, such as an unconfirmed child exit or agent withdrawal, stops it there;
- one teardown: whatever ends the session unwinds into that job's `finally`, which runs in the background
  and is the only place resources are withdrawn;
- a successor is accepted at once but waits for the previous cleanup before it authorizes or acquires
  anything, through a process-wide reference that survives the service being destroyed and recreated;
- withdrawal acts only on the session its own job published, so a late finalizer cannot take back a
  successor's resources;
- cleanup that could not be confirmed does not stop the service: while it is alive the service stays
  foreground and the next command retries what remains, with no timer in between. `onDestroy` makes one
  final attempt and then releases the service scope, which ends the automatic retries; a later command that
  recreates the service in the same process joins the process-wide predecessor and retries the inherited
  cleanup from there. What survives even that is listed under
  [External State And Cleanup](#external-state-and-cleanup): a wedged child with its TUN descriptor, and the
  global preference.

## Platform Integration

### Privileged Shizuku Operations

Every Android control operation runs inside a
[`ShizukuEpoch`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/ShizukuEpoch.kt): an authorized
Shizuku identity, its wrapped binders, and the effective UID they act as. It brackets each transaction, so a
replaced or dead identity is caught before a result is believed.

ConnectivityService authorizes `releaseNetworkRequest` against the UID stored with the request, and does so
asynchronously, so a release issued under a different effective UID silently does nothing while the
app-facing wrapper reports success. Cleanup therefore requires an identity with the same effective UID as
the session that issued the request, and releases through the retained handle rather than
`unregisterNetworkCallback`, which would destroy the only handle a retry has.

The privileged `ConnectivityManager` is built without running a constructor, so the process-wide singleton
is never written and ordinary `Context` lookups keep returning the ordinary manager. The exact API inventory
is in [`mobile/src/hiddenApiStubs/README.md`](../../mobile/src/hiddenApiStubs/README.md) and the
compatibility assumptions in the root [`README.md`](../../README.md).

### Restricted TestNetwork

The implementation calls `TestNetworkManager.createTunInterface` and **never** `setupTestNetwork`, whose
setup path adds `NET_CAPABILITY_NOT_RESTRICTED` and would let any app with network access select the
network.
Publication is this app's own `NetworkAgent`, and the security-relevant part of its capabilities is:
`TRANSPORT_TEST` with the session's own `TestNetworkSpecifier`; no `NET_CAPABILITY_NOT_RESTRICTED`, no
`NET_CAPABILITY_TRUSTED` and no `NET_CAPABILITY_INTERNET`; an empty allowed-UID set; legacy type `TYPE_TEST`
and score 1, so the retained exact request is what keeps the agent wanted.

The interface contract is fixed for the life of a session, and the daemon is pinned to it:

| Property | Value |
| --- | --- |
| IPv4 | `192.0.2.1/30` |
| IPv6 | `2001:db8:1::1/64` |
| Virtual DNS | `192.0.2.2`, `fd00::53` |
| MTU | 1500 |

The IPv4 and IPv6 interface addresses come from the IPv4 and IPv6 documentation prefixes, because every
address the interface holds is unreachable for clients and those prefixes are guaranteed never to be
assigned; [`routing.md`](routing.md#rootless-shizuku-mode) says why that matters. The IPv6 interface address
must also look global: tethering copies only `isGlobalPreferred()` `/64`s from its upstream
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#239)),
and that predicate rejects ULAs
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/LinkAddress.java#487)),
so a ULA prefix would yield no client IPv6 at all. The virtual DNS addresses are not subject to that rule:
`192.0.2.2` sits in the same documentation prefix as the IPv4 interface address, while `fd00::53` is a
unique local address, which tethering admits because it applies a looser predicate to DNS servers than to
prefixes.

`LinkProperties` is built once and never mutated. Its addresses are re-read from the kernel so the published
set matches what `createTunInterface` really assigned, and it carries an IPv6 default route because
tethering requires one alongside the global `/64` before it delegates
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#210)).
The full contents are catalogued in [`routing.md`](routing.md#rootless-shizuku-mode).

### System Tethering Selection

This mode sets one global flag and observes the result. It sends
`ITetheringConnector.setPreferTestNetworks(true)` through a pinned connector rather than
`TetheringManager.setPreferTestNetworks`, because the manager discards the result code
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#2241))
and without `NETWORK_SETTINGS` the service reports `TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION` through the
listener instead of throwing, so a denial is otherwise silent. The call is `oneway`, so the answer arrives
separately under a deadline, and both writes go to the one pinned connector because `IBinder` orders one-way
calls only per object.

**Success only means the flag moved.** It is not a reselection trigger and never proof that tethering
selected this network: tethering holding an ordinary upstream keeps it until Android reevaluates or
something outside this mode cycles tethering. That is the ordinary case `RESTART_REQUIRED` reports.

On Android 13 the preference is consulted only when automatic upstream selection is on
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/src/com/android/networkstack/tethering/Tethering.java#1798)),
so a build that disables it can never run this mode and is refused before anything is created. Android 14
and later force automatic mode on.

Tethering-service death is terminal for this process, because `TetheringManager` caches its connector
permanently and AOSP states that no recovery is possible
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#467)).
The session ends, further sessions are refused with that reason, and the preference clear is discharged
rather than retried, since the flag died with the process that held it.

Two platform facts shape what the daemon sees. **Double NAT**: Android's forwarding and MASQUERADE run
before packets reach the TUN, so Android owns the inner mapping, filtering and timeouts and the daemon never
mirrors them. **Proxied DNS**: Android may originate forwarded DNS from a TestNetwork-local address, so
client DNS can arrive at the virtual resolver addresses from the platform rather than from the client.

## Session State

The state is recomputed from the global upstream observation for as long as the session runs. **Only
`ACTIVE` admits dataplane traffic**, as a rule carried by the applied configuration, so what the daemon
admits is whatever the last acknowledged configuration said.

| State | Meaning | Admits |
| --- | --- | --- |
| `ARMED` | Tethering names no upstream | no |
| `VERIFYING` | Tethering names an upstream this session cannot currently classify | no |
| `ACTIVE` | Tethering reports the exact `Network` this session published | **yes** |
| `RESTART_REQUIRED` | Tethering reports an ordinary upstream | no |

Ownership is decided by identity against the exact `Network` the agent returned, never by a capability read.
Classifying somebody else's upstream needs no privilege: `getNetworkCapabilities` enforces
`ACCESS_NETWORK_STATE` alone and does not redact transports, so a non-TEST upstream is `RESTART_REQUIRED`, a
`TRANSPORT_TEST` one is a terminal collision, and a network that disappeared before the read stays
`VERIFYING`. The row shows a label only while the job that committed it is still the accepted one, so a
label can never outlive its own session.

## Startup, Stop And Rollback

Startup has four phases. A stop can cancel it while it waits on the previous cleanup or acquires the new
session, though the cleanup in phase 1 finishes once it has begun, and any failure or cancellation reaches
the same rollback.

1. **Previous cleanup.** Retry whatever the last session still owed - which can mean terminating a child,
   closing the TUN, withdrawing the agent, releasing the request and clearing the preference - ahead of
   every prerequisite below, since none of those is what cleanup was missing while any of them can refuse a
   new session permanently. One command makes one attempt.
2. **Prerequisites.** Authorize Shizuku; on Android 13 require automatic upstream selection; refuse to start
   while a session is still recorded, or while any `TRANSPORT_TEST` network exists. The collision scan runs
   before this session registers an agent, so it cannot match itself.
3. **Publication.** Resolve the reflective members cleanup will need, so a member the installed module does
   not expose refuses the session before anything is created; create the TUN; register the upstream and
   egress observations and await the first upstream value, before either of the two mutations that can move
   tethering's upstream; spawn the child and complete the
   bootstrap handshake ([`lifecycle.md`](lifecycle.md#app-uid-bootstrap)); register the exact request,
   acquire the pinned connector and link its death recipient, then set the preference, in that order,
   because connector death silently undoes the preference; register and connect the agent and validate the
   capabilities and `LinkProperties` that come back.
4. **Commit.** Publish a state, send the first configuration, and start watching for the terminal events
   below.

Ownership of the child begins at spawn, not at a completed handshake, because a failure after the
descriptor transfer can leave the child holding its copy. The request, the preference and the agent are
recorded as owed from the moment their transaction is issued, because for these a thrown exception is not
proof that nothing happened; each is then classified from the handle, `Network` or result code the platform
returns. What cannot be classified stays `UNKNOWN`, which withdrawal treats as still existing.

The ordered withdrawal is the same for a stop, a failed publication and an autonomous ending, is idempotent
and resumable, and is non-cancellable. Its order is what the platform requires:

1. stop the session's observers, then ask the daemon to close admission and await the acknowledgement; if
   that cannot be obtained, terminate the child immediately instead of leaving it admitting through the next
   step's minute-long deadline;
2. clear `preferTestNetworks` through the still-live connector, then unlink its death recipient, since this
   is the one piece of system state that outlives a session;
3. terminate the child and confirm its exit ([`lifecycle.md`](lifecycle.md#app-uid-bootstrap)); everything
   after this step assumes the child is gone;
4. withdraw the agent and prove the network is gone before releasing the request, because
   ConnectivityService emits the request's loss when the agent disconnects, before it removes the network,
   so unregistering the callback first would lose that proof. `onNetworkUnwanted` is always owed;
   `onNetworkDestroyed` is owed once `onNetworkCreated` arrived, and the request's `onLost` once it reached
   `onAvailable`. Agent unregistration deliberately runs outside the Shizuku identity, so it still works
   after Shizuku death;
5. close the descriptor - never earlier, or a successor would relay through a TUN ConnectivityService still
   exposes;
6. release the exact request on the retained handle, then clean the framework's callback bookkeeping.

Withdrawal reports itself finished only when the descriptor, the child and the agent are proven gone. Three
outcomes are not equally recoverable:

| Outcome | Session result | Recovery |
| --- | --- | --- |
| Privileged release issued but unconfirmed | over; successor refused meanwhile | a later command retries it before anything is created |
| `UNKNOWN` exact request | over; no native network implied | no in-process recovery; process death required, and no further session runs before then |
| `UNKNOWN` agent | withdrawal reported unfinished; process kept | no in-process recovery; process death required, since a native network may exist that this process cannot name |

A committed session also ends on its own when it loses the tethering connector, the daemon control
conversation, the agent, the exact request, or reports its own failure. Whichever occurs first is shown to
the user; the withdrawal above then runs in the same finalizer a stop would have used.

## Upstream Generation And Downstream Epoch

Two independent version numbers travel in every configuration, and the daemon refuses one whose values
disagree with the fields they retire.

**`upstream_generation`** says which `Network` the egress sockets are bound to. It advances on every change
to the selected upstream, including a `LinkProperties` change that leaves the handle equal, because the
state pinned behind that handle is stale just the same, and including an interface-index change with no
observation behind it. It is deliberately not the handle, since netIds are reused. The handover it drives is
in [`lifecycle.md`](lifecycle.md#selected-network-handover).

**`downstream_epoch`** says whether a TUN-visible tuple still names the same client, and everything keyed by
such a tuple carries it. It advances on any loss of positive confirmation that tethering is still carrying
this exact network, because tethering can rebuild its inner NAT behind an unchanged handle.

Egress is the app UID's own default `Network` and nothing else. Root mode's upstream preferences take no
part in it, the session's own TUN is rejected by interface name so the daemon cannot relay to itself, and
**there is no fallback network**: having no selectable `Network` is a steady state in which upstream work
fails per operation and the session resumes on the next selection.

## App-UID Dataplane

The child is the same `vpnhotspotd` binary root mode uses, exec'd in place from the APK at the app UID. It
receives a configuration stream instead of the root call/reply conversation: the newest configuration is the
whole truth, and each is acknowledged only once whatever the change retires is really gone rather than
asked to go. Platform resolver work is the documented exception ([`dns.md`](dns.md)).

Every packet read from the TUN is untrusted input from an unknown local principal. Destinations are compared
against the session's exact virtual-address set before attribution, reassembly or transport dispatch. The
three principals - DNS, IPv4, IPv6 - are shared classes; nothing derives identity from a source address.

| Client traffic | How it is carried | Outer state the daemon owns |
| --- | --- | --- |
| TCP | terminated locally by `smoltcp` and reconnected upstream, so each side segments to its own MTU | flow record, socket, two 64 KiB buffers, one upstream descriptor |
| UDP | one endpoint-independent, address-filtered mapping per TUN-visible source, on an unconnected socket reused across destinations | mapping, remote records, bounded send history |
| DNS to a virtual resolver address | terminated and handed to the platform resolver, over UDP and TCP | transaction row, logical resolver token |
| ICMP Echo | relayed on a ping socket | Echo session and socket |
| ICMP errors | translated only where the daemon can prove what they describe | none |
| Anything else | dropped | none |

Those UDP semantics are the outer ones only: Android's inner NAT sits in front of them and is not mirrored,
so nothing here promises end-to-end mapping or filtering behaviour for a physical client. Link-scoped
destinations - multicast, broadcast, link-local, loopback, unspecified - are dropped, while private and
unique-local addresses are ordinary destinations for a VPN or a NATted upstream and are relayed. Translation
rules for received ICMP errors live with the code in `shared/icmp_translate.rs`; [`errors.md`](errors.md)
owns report semantics and [`dns.md`](dns.md) the resolver handoff.

### Packetization Bounds

MTU 1500 is immutable in the agent's `LinkProperties` and in the daemon, and is also the floor relayed IPv4
output is sized against, sent as `downstream_mtu_floor`. This mode owns no downstream, so nothing narrower
can be measured and nothing can move the floor within a session; the field may still only change together
with `downstream_epoch`, because every queued packet was sized against it. A narrower downstream link is
therefore a signalled path-MTU event - Android's forwarding path answers ICMP Fragmentation Needed at the
TUN - rather than a black hole. Path-MTU signalling toward clients comes from `EMSGSIZE` and the socket
error queues, never from a configured upstream MTU, because a handover can change that.

A datagram within the floor goes out whole and is issued no fragment identifier. A larger one, such as a
multi-kilobyte DNS reply, is source-fragmented in both families. Newly originated TUN-side packets use hop
limit 64; relayed traffic and translated errors preserve validated received hop metadata. All TUN writes
pass through one packet writer with bounded queueing, atomic packet writes and final size validation.

**IPv4 Identification nonreuse.** Every fragment of one fragmented IPv4 datagram carries the same
`(source, destination, protocol, Identification)`, and a different guarded datagram may not reuse that
identity until 60 seconds after the previous datagram's latest successful fragment write. Guarded output is
denied and counted while a tuple has no reusable value, including for the first 60 seconds of a session,
which covers what a predecessor may have written just before it stopped. IPv6 fragment headers take a
session-wide wrapping sequence and carry no such rule.

Ingress reassembly is bounded in both families by overlap, extension, length and timeout rules; IPv6
extension-header parsing is bounded and refuses forbidden chains.

### Resource Bounds

One owner decides whether traffic-driven state may exist, accounting for descriptors and bytes separately,
since they do not substitute for one another. Both totals are measured at session start rather than chosen:
descriptors from `RLIMIT_NOFILE` and `/proc/self/fd`, bytes as a conservative eighth of the kernel's
`MemAvailable`. The byte total is a policy share rather than a process ceiling; it counts Rust-visible owned
heap only, and row counts bound what it cannot see. Two floors sit inside those totals - descriptors held
back for DNS, bytes held back for essential work - and general traffic cannot reach into them. The DNS floor
is an eighth of the platform's 256-query per-UID limit, because the platform's own proxy and the app process
share those slots; exhaustion answers SERVFAIL.

Three rules hold the accounting together, and they are what other owners may rely on:

- **Deny new, never evict live.** Established transport state is never retired to admit new work.
- **Cancel, join, close, then return the reservation.** A reservation never releases itself on cancellation,
  in a `Drop` or on an error path, because each of those runs before the resource is actually gone. An
  acknowledged configuration therefore means the descriptors of everything it retired are closed.
- **One reservation per row, taken before the payload**, which is what makes a bounded queue a real bound.

Because there is no client identity, these budgets are self-protection rather than fairness.

### Idle Floors And Timers

These govern only the daemon's own outer state; Android's inner NAT keeps its own conntrack, none of which
is mirrored, configured or timed from here.

| State | Floor |
| --- | --- |
| UDP mapping and remote records | 300 s idle |
| UDP error history record | 60 s absolute; never refreshed |
| TCP established, and the half-closed phases that can still carry data | 7,440 s idle |
| TCP before a connection is made, and once neither direction has anything left to carry | 240 s idle |
| Echo session | 60 s idle |
| IPv4/IPv6 incomplete reassembly | 60 s |
| IPv4 Identification nonreuse | 60 s from the write |

Outbound UDP activity refreshes only its own mapping and remote; inbound, rejected and error packets do not.
Once admission is closed the session may use existing state but creates and refreshes nothing. Stopping is
not pausing: the deadlines keep running and what they retire is still taken back, with a reset for a flow
that has a remote endpoint and silence for one that has none.

#### Outer TCP Phases

The two TCP floors above come from RFC 5382 section 5, REQ-5, and so does the classification of which
phase gets which, applied to the post-action `smoltcp::socket::tcp::State`:

| `State` | Outer idle floor |
| --- | --- |
| `Listen`, `SynSent`, `SynReceived` | 240 s |
| `Established`, `FinWait1`, `FinWait2`, `CloseWait` | 7,440 s |
| `Closing`, `LastAck` | 240 s |
| `TimeWait` | none; `smoltcp`'s own `CLOSE_DELAY` owns it - ten seconds in the pinned 0.13.1, not a host's 2MSL |
| `Closed` | terminal |

`FinWait1`, `FinWait2` and `CloseWait` keep the established floor because one direction can still carry
application data in each. Activity rearms the current phase's whole floor; nothing the daemon itself
originates rearms anything. **No post-RST retention is claimed**: RFC 7857 recommends holding a mapping for
four minutes after a matching reset, and this daemon does not - `Closed` is terminal and a reset from either
side ends the flow.

#### A Flow Can Outlive Its Worker

Both TCP workers return as soon as their own ordered work is done, while the client's socket is typically
still in `LAST-ACK`, `CLOSING` or `TIME-WAIT`. A clean terminal from a flow nobody asked to stop therefore
**detaches** the flow instead of ending it: the worker and its upstream descriptor are gone, while the flow
keeps its socket, buffers and reservation until `smoltcp` reaches `Closed`, its outer floor passes, a
configuration retires it, or the session ends. Its record is removed and its reservation released exactly
once, by whichever happens first, and no retirement waits for a second terminal from it. A cancelled worker, a
socket that never completed its handshake, and a failed worker are not this case: each has no client teardown
left to protect.

## External State And Cleanup

This mode issues **no** root-daemon command, netlink write, netfilter rule, `ndc` request or sysctl change,
and it cannot: every one needs root, while both the app and its daemon run at the app UID. Root's
`CleanRoutingCommand` neither creates nor removes anything here, and neither mode disturbs the other.

Issuing no command is not the same as causing no external state. Registering the agent materializes a native
network inside ConnectivityService, and Android tethering *selecting* it materializes forwarding, NAT, a
delegated prefix and netd counter entries against the session's `testtunN` - none of which this mode
requests, configures, or cycles tethering to obtain or release. Every item's trigger, owner, stop behaviour
and process-death behaviour is catalogued in [`routing.md`](routing.md#rootless-shizuku-mode). Three
feature-level facts belong here:

- **App-process resources need no Clean path.** The TUN descriptor the app holds, the agent and its native
  network, and the exact request are released by the ordered stop, by a rolled-back startup, or by the app
  process ending, without depending on a database, a preference or daemon memory surviving.
- **Process death is not a substitute for the ordered stop.** It releases only what the app process itself
  owns. The child is not bound to its parent by the platform: control-socket EOF makes a healthy child exit
  and drop its own independent TUN descriptor, but nothing signals a wedged one, so both the child and that
  descriptor can survive the app. The global `preferTestNetworks` flag is likewise left set.
- **The netd counter residue must not be removed.** Counter-chain state naming each dead `testtunN` was
  observed accumulating on the qualified Android 17 device and cleared there by a reboot. It lives in a
  chain netd owns alongside rules for live interfaces, so deleting it from the app side is exactly the
  delete-by-shared-family that [`routing.md`](routing.md#guardrails) forbids.

**Stop and reapply is not universal recovery.** An `UNKNOWN` agent or exact request needs process death, and
tethering connector death needs an app restart.

## Security Boundary

The posture is **best effort by design**: this mode protects tethered clients' traffic against the network,
and does not defend against other apps on the same device.

Device qualification established both halves. Restricted-network enforcement holds for `Network` handles: an
ordinary UID cannot select this network through Java or the NDK, bind a process or socket to it, set its
`SO_MARK`, or use DNS or ping sockets through it, and an unrestricted request never matches the agent. It
does not hold for the interface: packet capture on the TUN showed that **any app with network access that
can name the interface can put packets on it**, because binding a socket to an interface by name, and
selecting an output interface through IPv4 or IPv6 packet-info ancillary data, both address the Linux
interface directly and consult no capability. Two limits do survive: `SO_MARK` stays restricted, and the
socket's UID still selects Android's UID-range routing rules on top of whatever interface was chosen.

The impact is **one-way**. Any app with network access can push TCP and UDP out through the selected upstream
and impersonate a tethered client, so an app a VPN excludes by per-app rules can reach the internet through it
anyway. It cannot read tunnel traffic, because the TUN descriptor belongs to the daemon, and it has no return
path, because replies are routed downstream. Preventing the injection needs privilege this mode does not have:
a netd firewall mutation requires `NETWORK_STACK`, and a separate namespace, `tc`/eBPF or an OUTPUT owner
match requires root or system authority. The dataplane is therefore built to stay sound under injection -
parsing, reassembly bounds and hop-limit validation - and the root [`README.md`](../../README.md) tells users
plainly what this mode does and does not protect.

## Failure Semantics

[`errors.md`](errors.md) owns report shape and classification. Specific to a session:

- **Startup failures are terminal and enter the ordered rollback**: missing authorization, Binder,
  permissions or hidden-API access; native launch, authentication or descriptor-transfer failure; TUN,
  request, agent, preference, publication or security-readback failure; agent or request loss before commit;
  a foreign TestNetwork collision; and deadline expiry. Rollback usually leaves nothing behind, but a
  failure around an ambiguous Binder publication can end in one of the unknown or residual outcomes
  tabulated above.
- **A committed session ends** when the control conversation cannot carry or confirm an update, the TUN or
  writer becomes unusable, the agent or exact request is lost, or the tethering connector dies.
- **Local and optional failures do not end a session**: losing Echo or ICMP translation for one family,
  dropped malformed or unsupported packets, a resolver `EBUSY` answered as SERVFAIL, or a nonfatal report
  about the daemon's own state. An admission invariant violation is counted and reported, never fatal;
  treating it as fatal would let forged input stop a session.
- **Rollback never stops Android tethering**, and it cannot remove the `TestNetworkService` singleton that
  the first acquisition creates inside the system server.

## Qualification Status

Device behaviour is qualified by hand; this repository has no instrumented tests. Anything not listed below
is explicitly unproven, and the *Topology* column says what each result came from. These passes used Android
17 debug builds, except the security row, which is separate historical evidence from its own harness.

### Verified

| Area | What was established | Topology |
| --- | --- | --- |
| Publication and selection | Restricted `TRANSPORT_TEST` publication with its exact specifier and neither `INTERNET` nor `NOT_RESTRICTED`; Android selecting the session network as the tethering upstream; the delegated `/64` and MTU 1500 reaching the downstream | rooted with root-backed Shizuku, and stock non-rooted with shell-backed Shizuku; USB tethering |
| Attribution | Both branches of the effective-identity path: root-backed Shizuku, and shell-backed Shizuku on a stock non-rooted device | one device each |
| Fail-closed `ARMED` | Packets injected into the TUN while `ARMED` were dropped and created no dataplane state | rooted with root-backed Shizuku; direct TUN input, no external client |
| Dual-stack client traffic | IPv4 and IPv6 ICMP, UDP DNS in both families, and IPv4 and IPv6 HTTPS from a real external client | rooted with root-backed Shizuku, and stock non-rooted with shell-backed Shizuku; USB tethering |
| Selected-VPN egress | With a full-tunnel VPN applied to the app UID and root routing absent, client traffic passed while both the TestNetwork and the VPN interface counters advanced | rooted with root-backed Shizuku, USB tethering |
| VPN handover | Existing TCP flows reset in both transition directions as designed and fresh connections immediately succeeded; three VPN off/on cycles under UDP and DNS load left no stale residue and no resolver `EBUSY` | rooted with root-backed Shizuku, USB tethering. Not tethering start/stop cycles |
| Root coexistence | With both modes up, root's own policy rules sat ahead of Android's tethering upstream rule and won automatically; starting or stopping either mode changed no state of the other | rooted with root-backed Shizuku, USB tethering |
| Bounded dataplane | IPv4 path-MTU behaviour, 8 KiB IPv4 and IPv6 fragmentation, TCP half-close, NXDOMAIN, malformed-DNS recovery, and 16 concurrent 512 KiB downloads with no fatal, admission or invariant failure | rooted with root-backed Shizuku, USB tethering, one external client |
| Failure injection, partial | Killing the app-UID daemon while the app stayed alive produced prompt network removal and session retirement instead of a deadline-length stall. Force-stopping the app removed the network with the process, but ran no finalizer, so `preferTestNetworks` was left set | rooted with root-backed Shizuku, USB tethering |
| Cleanup | Ordered cleanup on normal stop, leaving no network, request, TUN, child or foreground service | rooted with root-backed Shizuku, and stock non-rooted with shell-backed Shizuku |
| Security boundary | Restricted `Network`-handle selection is enforced against a separately signed app and against the owner UID alike; direct interface injection succeeds. Isolation was tested and **failed** | separately signed out-of-repository attacker harness plus TUN packet capture, distinct from the rows above and proving nothing about release behaviour |

## Related Code And Documentation

- [`mobile/src/main/java/be/mygod/vpnhotspot/shizuku/`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/) -
  session ownership, ordering, the Shizuku identity, the privileged manager, the agent, the pinned tethering
  connector and the child conversation;
  [`ShizukuTetheringService.kt`](../../mobile/src/main/java/be/mygod/vpnhotspot/ShizukuTetheringService.kt)
  owns the job and the process lifetime behind it.
- [`mobile/src/main/rust/vpnhotspotd/src/`](../../mobile/src/main/rust/vpnhotspotd/src/) - the app-UID
  dataplane; see the source map in [`README.md`](README.md).
- [`routing.md`](routing.md#rootless-shizuku-mode) - the external-state and cleanup catalog;
  [`lifecycle.md`](lifecycle.md#app-uid-bootstrap) - bootstrap and
  [`#selected-network-handover`](lifecycle.md#selected-network-handover) - handover;
  [`dns.md`](dns.md), [`errors.md`](errors.md), [`traffic.md`](traffic.md) and
  [`invariants.md`](invariants.md) - resolver handoff, reports, accounting scope and cross-module rules.
- [`daemon.proto`](../../mobile/src/main/proto/daemon.proto) - the wire schema, including
  `ShizukuSessionConfig`;
  [`mobile/src/hiddenApiStubs/README.md`](../../mobile/src/hiddenApiStubs/README.md) and the root
  [`README.md`](../../README.md) - API inventory and compatibility assumptions.
