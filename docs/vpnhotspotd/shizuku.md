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
  bounded set of translated ICMP errors. AH, ESP, GRE, SCTP, unknown IP protocols and downstream link control
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
| [`AppUidDaemon`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/AppUidDaemon.kt) | the launched child, its authentication, and the RPC conversation with it |
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

Authorization itself can wait on the user's own permission dialog for as long as they take, and three things
end that wait: their answer, the lifespan being cancelled by a stop, or the exact Shizuku publication the
request was issued against being superseded. The last is compared by publication identity rather than by
binder equality - a redelivery of the same binder counts - and it is terminal for the *authorization* rather
than for the answer: what has gone is the identity the request belonged to, so even a grant could no longer
produce an epoch. Nothing about a successor has been validated, so supersession fails the authorization and
never re-asks against it: re-asking is a new authorization rather than that one continuing.

The answer itself is not publication-scoped. Shizuku hands every service the same process-global
`IShizukuApplication` and dispatches every permission result to one process-global listener list, so a
replaced-but-live service can still answer a request an attempt has already given up on - and answer it after
a successor has asked its own question. Every attempt therefore takes a distinct request code and accepts no
other, and accepts even its own only while the publication it was issued against is still current. An
abandoned attempt's late answer completes nothing, whether it reaches a successor's listener or its own, and
which of the two endings an attempt reports does not depend on when its owner gets around to waiting: the
publication swap and the result delivery both land on Shizuku's handler, so one strictly precedes the other.

The request and the app's own authorization check are both dispatched through the pinned publication's raw
binder rather than through `Shizuku.requestPermission`/`Shizuku.checkSelfPermission`, which resolve Shizuku's
mutable current service and cache the grant across replacements. A publication already known stale - by
publication identity, by Shizuku's own binder having moved, or by that binder being dead - issues no request
at all, and a grant is only accepted once that same publication answers the check again. Shizuku exposes no
way to retract a dialog it has already launched, so the window between that check and the transaction is
irreducible, and so is the interval between the last cancellation check and that same transaction: a
replacement or a stop landing inside either can leave a dialog whose answer this app will refuse, and the
user may see a second one when they start the row again.

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
separately, and exactly three things end that wait: the result, the tethering process's death, or the owner
cancelling the session. Death is raced because it is terminal rather than because it is a bound - the
listener would answer through a process that has gone - and it is classified as **positive proof that the
flag is discharged**, exactly as a committed session's own death watcher classifies it: `mPreferTestNetworks`
lives in the process that died and a restarted service begins `false`, so nothing is left to clear. Only an
epoch change or a caller going away leaves the flag unknown and still owed. Both writes go to the one pinned
connector because `IBinder` orders one-way calls only per object.

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
It is therefore one latched fact owned at the connector boundary rather than one per session, and it is
completed by every way this app can observe that death synchronously:

- a death recipient firing, on any connector this process linked;
- a connector delivery whose `linkToDeath` refuses a binder that has already gone, which is answered as the
  acquisition's own failure because AOSP holds consumers in a wait queue and its drain catches
  `RemoteException` per consumer and moves on
  ([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#480),
  [source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#365)),
  so an exception left to escape there would resume nothing at all;
- `unlinkToDeath` answering `false`, which Binder defines as the target having already died with its
  recipient "has been (or soon will be) called"
  ([source](https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-17.0.0_r1/core/java/android/os/IBinder.java#375),
  [source](https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-13.0.0_r1/core/java/android/os/IBinder.java#351)).
  In the second half of that the notification is still in flight against a connector withdrawal is about to
  drop, so reading the Boolean is what keeps a successor from passing the startup gate ahead of the callback.

Reading that latch is what refuses a later session; awaiting it is what ends each of the three waits only a
live tethering process could otherwise end - the preference result, the first upstream observation a startup
needs, and a committed session's own terminal watcher.

**A latch known complete settles the preference before anything else proceeds or returns.** Every owner that
touches the flag asks it at its own boundary rather than once at the start, because the death can become
knowable at any of them:

- the preference call asks it first on every failure, ahead of even the result code. A nonzero code is
  authoritative that *that transaction* did not mutate, and it restores the debt it came from - but a debt
  restored to live is a flag in a process that has gone, so the latch supersedes the restoration. The same
  death also arrives as a plain throw out of the `oneway` send, which is why the latch rather than the shape
  of the failure is the question;
- a withdrawal's cleanup attempt asks it again *after* unlinking its connector, since `unlinkToDeath`
  answering false is the last synchronous chance to learn of the death and it happens while that attempt is
  still unwinding. So a clear that failed unknown and only then proved the death is discharged in the same
  attempt, and the exact request's own release still runs - that release is authorized by ConnectivityService
  and owes tethering nothing. A cancellation is still propagated as a cancellation, after the discharge.

No known death is reported as residual debt, and none defers settlement to a second command. A failure with
*no* observed death does leave the flag `UNKNOWN`, on purpose: clearing is idempotent, so retrying an unknown
clear costs nothing while a wrong discharge cannot be taken back. A death with nothing in flight and no
session simply latches; there is no debt to settle. What the latch never carries is a *session's* debt: that
is the ledger's, and it is the process-level fact that outlives it.

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
   once tethering-service death has been latched in this process, while a session is still recorded, or
   while any `TRANSPORT_TEST` network exists. The collision scan runs before this session registers an
   agent, so it cannot match itself.
3. **Publication.** Resolve the reflective members cleanup will need, so a member the installed module does
   not expose refuses the session before anything is created; create the TUN; acquire the pinned connector
   and link its death recipient, which has to come before the wait below because that death is the only
   thing other than a stop that can end it; register the upstream and egress observations and await the
   first upstream value, raced against that death, before either of the two mutations that can move
   tethering's upstream - a callback registration or collection failure before that first value fails the
   startup rather than staying isolated in the session's observer scope; spawn the child and complete the
   session start call ([`lifecycle.md`](lifecycle.md#app-uid-session-start)); set the preference through that
   same already-linked connector, since connector death silently undoes the preference and one session links
   one recipient, and ahead of the exact request because that wait has no bound of its own and would otherwise
   run inside the request's lifetime; register the exact request, with the one-minute lifetime below; register
   and connect the agent and validate the capabilities and `LinkProperties` that come back, racing every one
   of those barriers against that request's expiry.
4. **Commit.** Publish a state, send the first configuration, and start watching for the terminal events
   below.

Ownership of the child begins at spawn, not at a completed start call, because a failure after the
descriptor transfer can leave the child holding its copy. A child that exits before it ever connects fails
the start with its exit status and captured output rather than leaving the start waiting on a connection
nothing will ever make, and passes through the same withdrawal; once it has connected and been authenticated,
what its own conversation says takes precedence over what its exit says. The first configuration is sent by the startup itself, so a daemon that
refuses it fails the start with that structured refusal rather than with a message of the app's own. The
request, the preference and the agent are
recorded as owed from the moment their transaction is issued, because for these a thrown exception is not
proof that nothing happened; each is then classified from the handle, `Network` or result code the platform
returns. What cannot be classified stays `UNKNOWN`, which withdrawal treats as still existing.

**A native network ConnectivityService cannot create is the one publication failure it announces to nobody.**
When `createNativeNetwork` fails - netd or DnsResolver refusing the netId -
[`updateNetworkInfo`](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#13527)
logs and returns, sending no agent callback at all, so `onNetworkCreated` never arrives, the request never
matches, and every barrier phase 3 waits on would wait forever. The exact request is therefore registered
through the timed `requestNetwork` overload, and its `onUnavailable` is the negative terminal each of those
barriers is raced against - with failure priority, so an expiry sharing a turn with a queued positive result
still wins rather than committing a result its request can no longer own. This bounds the *request*, not the
wait: it is the platform's own request lifetime,
[`notifyNetworkAvailable`](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#13699)
cancels it before dispatching availability so a published session can never be taken back by it, and no other
Shizuku or tethering wait in this mode has an elapsed-time terminal - those still end on their answer, on the
tethering process dying, or on a stop. The value is borrowed rather than derived, because the platform states
no bound on native network creation: one minute is `TetheringManager`'s own `DEFAULT_TIMEOUT_MS`, the bound it
puts on its synchronous service-result and initial-callback waits
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#546)),
at both ends of the supported range. That is a precedent for the length and nothing more, and it is not a
bound on this session's preference call, which bypasses `TetheringManager` altogether and waits on the
connector's direct result with no elapsed-time terminal - which is why that call is issued before this
lifetime starts.

Expiry is an ordinary startup failure and enters the same ordered rollback, with one difference: it settles
the exact request's own debt on the spot instead of leaving it to that rollback. ConnectivityService removes
the request before it sends the callback and `ConnectivityManager` drops its `sCallbacks` entry and tombstones
the callback before invoking it, so both halves are proven gone, no cleanup identity is needed, and no
successor is forbidden by it. Everything else the session had reached - the preference, the child, the agent,
the TUN - is still owed and is withdrawn in the order below. What no app can clean up is ConnectivityService's
own residue: `createNativeNetwork` is not atomic, and a network destroy is
[guarded](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#6079)
on a creation that by definition did not complete, so netd state from a half-finished create is left for the
platform to reconcile. It is invisible to public callbacks, this app never held it, and it is not what the
rollback is for.

The ordered withdrawal is the same for a stop, a failed publication and an autonomous ending, is idempotent
and resumable, and is non-cancellable. Its order is what the platform requires:

1. stop the session's observers, then ask the daemon to close admission and await the acknowledgement; if
   that cannot be obtained, fence the child immediately instead of leaving it admitting through the next
   step's wait on the tethering service;
2. clear `preferTestNetworks` through the still-live connector, then unlink its death recipient - reading
   that unlink's answer, since a refusal is itself the death - because this is the one piece of system state
   that outlives a session;
3. terminate the child and confirm its exit ([`lifecycle.md`](lifecycle.md#app-uid-session-start)); everything
   after this step assumes the child is gone;
4. withdraw the agent and prove the network is gone before releasing the request, because
   ConnectivityService emits the request's loss when the agent disconnects, before it removes the network,
   so unregistering the callback first would lose that proof. `onNetworkUnwanted` is always owed;
   `onNetworkDestroyed` is owed once `onNetworkCreated` arrived, and the request's `onLost` once it reached
   `onAvailable`. Agent unregistration deliberately runs outside the Shizuku identity, so it still works
   after Shizuku death;
5. close the descriptor - never earlier, or a successor would relay through a TUN ConnectivityService still
   exposes;
6. release the exact request on the retained handle, then clean the framework's callback bookkeeping. A
   preference clear still owed at this point is reissued under a cleanup identity first; if anything in that
   attempt proves the tethering process gone - the acquisition, the call, or the unlink after it - that debt
   is discharged and this release still runs, because it is authorized by ConnectivityService and has nothing
   to do with tethering.

Withdrawal reports itself finished only when the descriptor, the child and the agent are proven gone. Three
outcomes are not equally recoverable:

| Outcome | Session result | Recovery |
| --- | --- | --- |
| Privileged release issued but unconfirmed | over; successor refused meanwhile | a later command retries it before anything is created |
| `UNKNOWN` exact request | over; no native network implied | no in-process recovery; process death required, and no further session runs before then |
| `UNKNOWN` agent | withdrawal reported unfinished; process kept | no in-process recovery; process death required, since a native network may exist that this process cannot name |

A committed session also ends on its own when it loses the tethering connector, the daemon control
conversation, the agent, the exact request, or reports its own failure. Whichever occurs first is shown to
the user, and the operational exception is what travels there: a structured daemon report and a frame the
app refused are both shown as themselves, while a control stream that simply ended has nothing more specific
to say and shows the generic message. The withdrawal above then runs in the same finalizer a stop would have
used.

## Upstream Generation

One version number travels in every configuration, and the daemon refuses one whose value disagrees with the
fields it retires.

**`upstream_generation`** says which `Network` the egress sockets are bound to. It advances on every change
to the selected upstream, including a `LinkProperties` change that leaves the handle equal, because the
state pinned behind that handle is stale just the same, and including an interface-index change with no
observation behind it. It is deliberately not the handle, since netIds are reused. The handover it drives is
in [`lifecycle.md`](lifecycle.md#selected-network-handover).

It is the only retirement stamp there is, and there is deliberately no downstream counterpart. Android's own
conntrack owns the mapping between a tethered client and the TUN-visible endpoint the daemon keys its state
by, so neither side can observe that such an endpoint changed hands, and neither a downstream transition nor
an `ACTIVE` one is evidence that one did. The two are not the same input, though, and only one of them
reaches a configuration at all.

**Downstream membership is not observed.** The app watches tethering's upstream and never its interface
lists, so a downstream joining or leaving the same upstream moves no configuration field - not even
**`admit`** - and produces no round trip.

**Losing or regaining `ACTIVE` moves `admit`, and never a retirement stamp.** `admit` is level-triggered
truth rather than an event, and closing it retires nothing: the daemon drops what it reads from the TUN,
creates nothing and refreshes no lifetime. That is not a pause. Deadlines keep running and protocols keep
finishing throughout, so what a reopened session resumes with is whatever independently survived the
interval, not a guarantee about any particular connection.

What a generation change *does* retire is narrower than "everything a client touches": the UDP mappings, Echo
sessions and ordinary TCP flows that hold a socket bound to the selection being left, plus the TUN output
already queued under it, which the writer drops at dequeue. Reassembly contexts and DNS-over-TCP transports
hold no such socket and are deliberately kept - the contexts expire on their own timer, and a transport's
answer from the retired selection becomes that query's own SERVFAIL while the connection carries on.

Egress is the app UID's own default `Network` and nothing else. Root mode's upstream preferences take no
part in it, the session's own TUN is rejected by interface name so the daemon cannot relay to itself, and
**there is no fallback network**: having no selectable `Network` is a steady state in which upstream work
fails per operation and the session resumes on the next selection.

## App-UID Dataplane

The child is the same `vpnhotspotd` binary root mode uses, exec'd in place from the APK at the app UID, and
it speaks the same call/reply/event/error conversation - `ClientEnvelope` in, `DaemonEnvelope` out - over a
command family of its own. Root's commands describe mutations that need root, so none of them is served
here and neither daemon serves the other's.

The start call carries the TUN and owns the session for as long as it runs; each configuration is an
ordinary one-shot call keyed to it ([`lifecycle.md`](lifecycle.md#app-uid-session-start)). Configuration
stays level-triggered - the newest configuration is the whole truth, and each is answered only once whatever
the change retires is really gone rather than asked to go. Platform resolver work is the documented
exception ([`dns.md`](dns.md)). What the shared lifecycle buys is attribution: a descriptor the daemon
refuses, a dataplane it cannot build and a configuration it will not accept each come back as that call's
own structured error rather than as a socket that closed.

Every packet read from the TUN is untrusted input from an unknown local principal. Destinations are compared
against the session's exact virtual-address set before attribution, reassembly or transport dispatch. The
three principals - DNS, IPv4, IPv6 - are shared classes; nothing derives identity from a source address.

| Client traffic | How it is carried | Outer state the daemon owns |
| --- | --- | --- |
| TCP | terminated locally by `smoltcp` and reconnected upstream, so each side segments to its own MTU | flow record, socket, two 64 KiB buffers, one upstream descriptor, a bounded byte bridge of one send buffer's worth each way, and a reserved terminal tail of one more |
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

MTU 1500 is immutable in the agent's `LinkProperties` and in the daemon, and is the one size everything is
built against: the TCP stack's advertised MTU, the DF decision for relayed IPv4 output, and the split point
for source fragmentation. It travels once, in `StartShizukuSessionCommand.mtu`, and is checked there against
the descriptor's own interface MTU before the dataplane is built; no configuration carries it, so nothing
within a session can move it. This mode owns no downstream, so there is nothing narrower to measure either. A
narrower downstream link is therefore a signalled path-MTU event - Android's forwarding path answers ICMP
Fragmentation Needed at the TUN - rather than a black hole. Path-MTU signalling toward clients comes from
`EMSGSIZE` and the socket error queues, never from a configured upstream MTU, because a handover can change
that.

A datagram within that MTU goes out whole and is issued no fragment identifier; relayed IPv4 output also
carries DF set, IPv6 having no such bit. A larger one,
such as a multi-kilobyte DNS reply, is source-fragmented in both families, under the guarded IPv4
Identification below or a 32-bit IPv6 sequence; if no IPv4 Identification can be issued the datagram is
dropped and counted rather than sent atomically. Newly originated TUN-side packets use hop limit 64; relayed
traffic and translated errors preserve validated received hop metadata. All TUN writes pass through one
packet writer with bounded queueing, atomic packet writes and final size validation.

**IPv4 Identification nonreuse.** Every fragment of one fragmented IPv4 datagram carries the same
`(source, destination, protocol, Identification)`, and a different guarded datagram may not reuse that
identity until 60 seconds after the previous datagram's latest successful fragment write. Guarded output is
denied and counted while a tuple has no reusable value, including for the first 60 seconds of a session,
which covers what a predecessor may have written just before it stopped. IPv6 fragment headers take a
session-wide wrapping sequence and carry no such rule.

Ingress reassembly is bounded in both families by overlap, extension, length and timeout rules; IPv6
extension-header parsing keeps Fragment headers for reassembly and refuses unsupported or source-routing chains.

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

A TCP flow's reservation covers every byte its pipes may hold, whether or not that flow ever fills them, so
the read-ahead is paid for in flows the device can admit rather than in memory it finds out about later. There
are three such pipes and each is a `tokio::io::simplex`: the two directions of the byte bridge, and the
reserved terminal tail the client's ending is extracted into. Their buffers are `BytesMut`, and tokio bounds
their *length* at the capacity asked for while `Vec`'s amortized doubling bounds their *allocation* at four
times it, so each is charged at four times what it will admit. At 64 KiB apiece that is 768 KiB of pipe on top
of a charge whose two stack buffers were already 128 KiB - about 900 KiB per flow, against roughly 70 KiB of
chunk buffers before.

The tail is charged at the same multiple as the other two rather than at half of it. An earlier derivation
halved it because the tail is written once from empty and never read while it fills; because the extraction
closes the main stream *first*, a worker that is keeping up starts draining the tail while the writes into it
are still happening. That is an ordinary producer and consumer, so it takes the ordinary bound. What it costs in prepared flows depends on the device: the solver takes the largest count whose
per-flow charge and tables still fit the *general byte headroom*, so where bytes are the binding constraint
the count falls in proportion, and where the descriptor floor or a table bound is what binds first it does
not move at all.

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

#### TCP Transfer, Backpressure And Wakes

One task owns the client-side stack, so both directions of every flow are moved by it. What a flow's worker
sees of that owner is one bounded Tokio stream and nothing else. It is built from library combinators only:
three one-way `tokio::io::simplex` pipes - downward and upward for steady-state traffic, and one **reserved**
for the client's ending - presented as a single `AsyncRead + AsyncWrite` by `chain` and `join`. The worker
reads `down.chain(tail)` and writes the upward pipe; it never learns there are three. Naming the directions
outright rather than `split`ting a `duplex` is what makes the tail orderable against the main stream at all.
Each `simplex` is itself a `split` and still owns one `Arc<Mutex<SimplexStream>>`; what it removes is the
*extra outer* split a `duplex` half needed to reach `chain` - one more allocation, and a second mutex every
worker read and write took before reaching the pipe's own. Ordinary TCP and
DNS-over-TCP are handed the same object by the same factory.
**No message travels between a flow's task and the engine to say work is waiting, in either direction, no
capacity is read, and nothing is shared between flows.** Every wake is the library's own: there is no
periodic poll, no timer-driven readiness check and no readiness protocol of this daemon's. (The owner does of
course *poll* in the `Future` sense - `poll_fn`, `poll_read`, `poll_write` - which is how a Tokio waker gets
registered in the first place; what it never does is ask again on a schedule.) An ordinary flow's worker runs
`tokio::io::copy_bidirectional_with_sizes`
between its upstream socket and that stream, at one 1,500-byte scratch per direction, so its half of the
transfer is Tokio's rather than this daemon's.

The engine reads the stream straight into `smoltcp`'s send buffer and writes `smoltcp`'s receive buffer
straight into the stream, at the contiguous slices each of them offers, so nothing is ever held between the
two. A short move is fewer bytes, never a remainder somebody has to remember.

Every stall a flow can be in is one of four, and each has exactly one wake:

| Stall | What frees it | What registers the wake |
| --- | --- | --- |
| the stream is empty and the client can take bytes | the worker writing one byte | the engine's `poll_read` on the stream |
| the stream is full and the client's receive buffer has bytes | the worker reading one byte | the engine's `poll_write` on the stream |
| the client's send buffer is full | the client's acknowledgement | the TUN read the ingress task already waits on |
| the client's receive buffer is empty | a packet from the client | the same |

The last two are deliberately not registered with `smoltcp`: those buffers change only when a packet or a
stack timer is processed, and the ingress task is that processing's only owner and re-enters the crossing
immediately after it. That is why `smoltcp`'s `async` feature stays off - `register_send_waker` and
`register_recv_waker` would fire into a task already scheduled to look.

Backpressure is the buffer filling, in both directions and losslessly. A full stream stops the worker reading
its upstream, which closes the *remote's* window; a full stream in the other direction stops the engine
draining the receive buffer, which closes the *client's* window. Up to one client-side send buffer of
read-ahead may sit in each direction, which is what lets the worker read while the engine is writing what it
read before. Nothing is dropped to relieve pressure.

Fairness is the pass rather than a scheduler: one pass over the round-robin order gives every live flow
exactly one turn, bounded by that flow's own charged buffers. **Ending the pass is what rotates it**, so the
flow that went first goes last in the next one and every flow reaches the client's send buffer first in turn.
The order is a single type that also admits, undoes and forgets a flow's place; there is no second path into
it, because both bugs that shape prevents were invisible at the call site - a pass that "rotated" by popping
and pushing every entry restored the order it began with, and a rollback that scanned for the candidate's
handle also deregistered a live predecessor holding the same one.

Three arrangements preceded this one. The first queued one chunk and waited for the engine to acknowledge
consuming it before reading again, which is a task/engine/task scheduling round trip per 1,500 bytes and no
read-ahead at all. The second kept the read-ahead but announced each chunk on a readiness channel every flow
shared, so one busy flow could make another flow's producer wait on it. The third replaced that channel with
a per-flow chunk queue, a one-chunk row, a deficit round-robin over the rows and a reserved channel slot in
the other direction - four pieces of custom protocol expressing what one bounded stream's own readiness
already says.

**DNS-over-TCP crosses the same stream.** It frames its answer once and writes the framed copy into the
stream; what bounds that write is the stream filling, which is the engine's own draining. Its delivery
reservation therefore pays for the answer and the framed copy, and no longer for a piece in flight - there is
no piece, and no acknowledgment for one.

**An abortive ending discards more than a one-chunk handover would.** A flow reset by a retirement, an idle
expiry or an upstream that failed or vanished drops whatever the engine has not yet written into the client's
send buffer, which is up to one stream's worth. The client is told the one way a terminated flow can say it:
a reset. The clean path loses nothing - the end of the stream is reported only after every byte written
before it, and a clean completion whose client is still open leaves the flow closing client-side so the
engine goes on delivering what the task left in the stream. That works because a worker shuts its write half
down before returning, not because it drops it - a `simplex` half signals nothing on drop - and because a
flow with a running task is cancelled and joined before this owner's own halves go.

#### The Client's Half-Close, And What A Closed Socket May Cancel

The client's FIN is propagated to the worker as the end of its own read stream, and **only once the receive
buffer is empty** - and because the main stream can be full for as long as the worker is slow, that emptying
may not be a *wait*. The client's ending is **extracted** instead, on the ingress that carried its FIN and
before that call returns: the main stream is closed, every remaining receive byte is moved into the reserved
tail, and the tail is closed once that buffer is empty. Because the worker is reading `down.chain(tail)`, what
it observes is always main bytes, then tail bytes, then exactly one end of stream - whichever order this owner
wrote them in.

**The idle floor is armed first, in the same call.** The rearm runs after the poll that produced the phase and
*before* the seal, with nothing awaited in between. That order is load-bearing rather than tidy: a successful
seal leaves the flow flushing, and a flushing flow in a terminal phase deliberately preserves the deadline it
already has, because `TIME-WAIT` has no floor of its own and `Closed` has a zero one. Seal first and what is
preserved is the *previous* deadline - so a FIN that lands a moment before one expires is cancelled while its
worker is still flushing bytes this daemon acknowledged. Rearm first and the preserved deadline is the fresh
one that FIN earned. An accepted reset is not rearmed at all: a flow whose client is gone has no idle floor
worth arming.

The extraction is **one uninterruptible step**, and that is the correctness property rather than an
optimisation. A resumable one was tried and was lossy for a reason that has nothing to do with the bridge:
this owner runs `Interface::poll` between owner turns - from the traffic path immediately after any pass that
moved something, and from the packet, timer, terminal and retirement paths besides - and a `TIME-WAIT` socket
ten seconds old clears its whole receive buffer inside that poll. A half-extracted ending is therefore not a
state to come back to; it is acknowledged client bytes waiting to be discarded by the next thing this owner
does, and this owner has five ways to do it. That is also why the extraction is not deferred to the next
traffic pass: the configuration arm is offered first and can await a whole retirement before that pass ever
runs.

What used to interrupt it was Tokio's own cooperative scheduling, not the pipe: `poll_proceed` answers
`Pending` once the *task* has spent its 128 operations, whatever the tail's state, and the engine scans every
live flow in one task poll - so a flow reached late in a busy pass hit that routinely.
`tokio::task::unconstrained` removes exactly that and nothing else: it sets the budget to unconstrained for
one poll of the future it wraps, and the future wrapped here is one shutdown, at most two writes and one
shutdown. What is exempted from cooperative yielding is a fixed, self-terminating step, not a loop that could
starve the runtime.

Everything else about it is bounded before it starts, and none of the bounds is this daemon's guess. The tail
is empty, because nothing else ever writes to it, and its capacity is the receive buffer's own, so the room is
there. `WriteHalf<SimplexStream>::poll_write` takes a *blocking* `std::sync::Mutex` rather than a poll-based
lock, so a worker reading the tail on another thread cannot make it answer `Pending`. Underneath it,
`SimplexStream` answers `Pending` in exactly one case - no room - and `poll_shutdown` is `close_write` and
`Ready(Ok(()))` unconditionally. A `smoltcp` receive ring is two contiguous runs at most, and the loop covers
both rather than writing once.

So with the budget out of the way, a `Pending` from the tail can only mean it was built smaller than the
receive buffer. That is a construction error, not a state: the flow is ended abortively and the crossing says
so. The alternative would be a clean close over a truncated stream, which is the one outcome that must never
be reachable.

Because the main stream is closed on entry, no later byte can overtake one already in the tail, and the
worker's `chain` puts the two back in one order.

**This owner's own FIN is withheld while the receive buffer still holds the client's bytes.** The question is
asked of the stack - `can_recv` - not of any state of the daemon's, so the invariant is structural: a
`CLOSE-WAIT` socket cannot reach `LAST-ACK` with unread client payload inside it. That was the first of two
real losses: the local FIN went out while a full main stream held those bytes in `smoltcp`, the socket reached
`Closed`, and the Closed-socket check cancelled the worker. The two half-closes stay independent otherwise -
from `ESTABLISHED` the client has not finished, so an upstream EOF is propagated at once, which the protocols
that need it depend on.

**The second loss was the other ordering.** Once the local FIN is out, the client's remaining payload and FIN
can put the socket straight into `TIME-WAIT` - smoltcp goes `FIN-WAIT-1` + FIN + ack-of-FIN there in one step
- and smoltcp's close timer is a fixed ten seconds that `set_timeout` does not govern, after which `reset()`
clears the receive buffer outright. Remembering that the ending was pending cannot preserve those bytes; only
getting them out of the stack can, which is what the reserved tail is for. Nothing about *scheduling* beats
that timer, and an earlier version of this document claimed otherwise: biasing the ingress `select!` toward
`tcp.attention()` only decides which arm is offered first, and every path that arm reaches - traffic, packets,
terminals, retirement - polls the stack itself. What beats the timer is that the extraction leaves no half-done
state for a poll to catch, so where the poll falls stops mattering.

That propagation is recorded as a state and not as a flag, because one thing reads it and the answer decides
whether bytes survive. The engine makes a **Closed-socket check** over its flows - distinct from the
configuration *sweep* that retires them, and never a substitute for it. Every non-packet caller makes it with
the poll it asks for; the packet path deliberately does not, and defers its single scan to the end of the
sequence, because the scan cancels workers and the decisions above it are entitled to run first. When the
client-side socket has reached `Closed`, that check cancels the flow's transport task, which is right for a
reset and for a flow that never opened. It is **wrong for a connection that ended the way its protocol says it
ends**: the task may still be writing bytes this daemon acknowledged to the client, held in the bridge or in
the copy's own scratch, and cancellation is abortive and would drop them - such a flow keeps running and its
later joined terminal is what reclaims it. And a flow already in the client-closing phase has no task to
cancel at all: its terminal was joined before it entered that phase, so `Closed` there is the owner's own
signal to reclaim and refund the row directly.

**A reset is therefore what the stack accepted, never what a bit said.** `smoltcp` accepts a reset in the
connected and closing phases alike and leaves exactly the `Closed` a completed shutdown leaves, so by the time
the check runs there is nothing left to tell them apart - and a client that resets *after* half-closing
cleanly is the ordinary case, not a corner one. But the `RST` bit in a header is only a **candidate**: the
checksum, the tuple, the sequence number and the window are all still the stack's to judge, and it refuses a
reset outright in `LISTEN`. Acting on the raw bit poisoned flows named by bad-checksum and out-of-window
segments, and the poison outlived the packet - each flow's own later, legitimate `Closed` then cancelled a
worker mid-flush.

So this owner carries no reset cause at all. It records which flow the segment names and what phase that
socket is in, lets the packet be pushed and processed, and reads the transition: `Closed`, or `SYN-RECEIVED`
back to `LISTEN`. Both polls run at **one pinned instant**, so a close timer that was already due cannot be
mistaken for a reset; a socket already `Closed` is never a candidate; and a packet the device refuses changes
nothing, because there is no cause to change. `smoltcp` never applies a reset segment's acknowledgement, so a
segment carrying `RST` can move a socket only through the reset arms themselves.

An accepted reset is **fenced synchronously**. `SYN-RECEIVED` -> `LISTEN` is a reset the stack accepted that
leaves a *reusable listener*, and cancelling the worker is not enough: cancellation is asynchronous, so a
same-tuple `SYN` arriving before that worker's terminal would attach to the predecessor - which reclamation
then tears down, destroying a connection the client had just made. The socket is aborted first, which leaves
`Closed` and accepts nothing, and only then is the worker cancelled. An established accepted reset is abortive
in the same way. Without any of this, a flow whose client is gone would hold its descriptor and its admission
slot for the whole of a clean flow's idle floor.

**No reset goes back.** The stack cleared the socket's tuple when it accepted the client's, so there is
nothing left to address one to, and this owner deliberately does not poll again on that path - a client that
has reset is answered with silence. The poll that *is* skipped there is the same one an ordinary segment
skips: after the push, a second same-instant `Interface::poll` runs only when this owner has left the stack
something to do, which means an extraction that emptied a receive buffer and reopened a window, or a fence on
a socket that still has an endpoint to send a reset to. An established flow's ordinary segment gets one poll,
which is what keeps the throughput path from paying twice.

**An opening is refused or unwound.** A `SYN` that also carries `RST` opens nothing: the stack refuses a
reset in `LISTEN` and would refuse the opening too, so admitting one would buy a socket, a grant, a bridge and
a spawned worker per packet for a segment that was never going to connect. And a `SYN` this owner *did* open a
flow for, which then never reaches the stack at all - the device still held an untaken packet - or which the
stack throws away when it does, on a bad checksum or a malformed header, leaves that socket only listening;
either way it is fenced and cancelled on the same call rather than held until the four-minute transitory
floor. Only the flow that packet created, so a duplicate `SYN` for a listener that already existed is left
alone.

**And the Closed-socket scan runs last.** It is what cancels a worker whose socket the stack has finished
with, so it may not run *inside* the poll the sequence above settles with: an accepted reset has to fence its
socket before its worker is cancelled, and a client's FIN has to arm its idle floor before its ending is
extracted, and a scan reaching a `Closed` socket first would do neither in that order. The settle primitive is
therefore the stack, the device and the output and nothing of any flow's; the scan happens once, after the
whole sequence has decided. Every other caller still reaches both together.

So a cleanly half-closed flow is not cancelled by `Closed` alone. It ends on its transport task's own
completion, which is ordinary work rather than a teardown to cut short. What still bounds it is unchanged and still
abortive: its outer idle floor, any configuration retirement, and the session's own shutdown.

**And the flush needs a bound that survives the teardown, in both close orderings.** A flow is somewhere in
three states, read from the bridge and the client-side phase *together* rather than from a flag beside them -
they move at different moments, and the gap between them is where this went wrong twice.

| State | What it is | What activity rearms it to |
| --- | --- | --- |
| ordinary | the client has not finished sending, or this is not a clean ending at all | the phase's own floor |
| **pending** | the stack has seen the client's FIN (`CLOSE-WAIT`, `CLOSING`, `LAST-ACK`, `TIME-WAIT`) and the extraction a few steps later in the same `accept` has not run yet | the established floor |
| **flushing** | propagated; the worker is writing what the client sent | the established floor while the phase can still reach the client, otherwise the existing deadline, preserved exactly |

*Pending* is a transient inside one `accept` and nothing longer-lived: it is what the rearm sees, because the
rearm runs before the seal in that same call. No flow can sit in it between packets - the device holds one
packet, that packet names exactly one flow, and the sequence seals that flow before returning; a timer or a
retirement moves this daemon's own side of a connection, never the client's, so neither can produce a peer FIN
for a later pass to discover. **When the upstream ends first**, this window is where a flow used to become
unbounded: this daemon has already closed its own side, so the client's
last payload and its FIN arrive together acknowledging ours, and smoltcp goes `FIN-WAIT-1` + FIN + ack-of-FIN
straight to `TIME-WAIT` in one step - whose floor is none at all. Ten seconds later `Closed` takes the stack's
own timer away too, and a worker blocked writing acknowledged bytes into an upstream zero window held its
flow, its descriptor and its admission for ever. Enough of those exhaust the fixed budget. A pending flow
therefore takes the **established** floor: the client's bytes are still on their way through this daemon,
whatever the client-facing half of the connection has torn down. That is an existing RFC 5382 figure, not a
new one.

*Flushing* splits, and that split is the other correction. `CLOSE-WAIT` and `ESTABLISHED` can still put
application data in front of the client - they are exactly what `smoltcp`'s `may_send` answers `true` for - so
a halted flow in one of them is an ordinary download whose client merely stopped sending, and every packet and
every delivery is real activity that rearms it. Freezing it there expired a long response that had never once
been idle, purely because it outlasted the floor its client's FIN happened to arm. Only the terminal phases -
`CLOSING`, `LAST-ACK`, `TIME-WAIT`, `Closed` - preserve the deadline the flush already has, because there no
byte can reach the client any more and the phase's own floor is none or zero: either would take the bound away
or make the flow immediately due and let the next expiry cancel the worker mid-flush.

The result is that a clean ending is bounded from the moment the stack sees the FIN, through every teardown
phase after it, in both orderings - and never by a figure invented for the purpose.

| How the client's side ended | Recorded as | What `Closed` does |
| --- | --- | --- |
| FIN, after every byte it sent was across, task still flushing | a clean half-close | nothing; the flow lives on until that task's joined terminal reclaims it |
| reset, whether or not a clean FIN came first | not a half-close; the cause outranks the phase | cancels the live transport task |
| never opened | not a half-close | cancels the live transport task |
| already closing client-side, task joined at its terminal | not a half-close | nothing to cancel; the owner reclaims and refunds the row |

#### Transport Completion And Client-Side Close

A flow's transport task completes when its own ordered work is done - its last bytes and its ordered end of
stream are *in the stream*, not delivered. An ordinary relay's task also closes the upstream descriptor
there; a DNS-over-TCP transport has none, it terminates locally, and the resolver transactions it asked for
are owned separately and can outlive it ([`dns.md`](dns.md)).

What follows depends on the client-facing connection at that moment:

- **it is still open** - past its handshake, not yet `Closed`, and the ending is a clean one nobody
  cancelled. The flow keeps its client-facing socket, its buffers, its reservation and whatever its half of
  the stream still holds, and the engine goes on delivering that, end of stream included;
- **it is already closed, never opened, cancelled, or the ending is a failure or a reported one.** There is
  no client-side close to protect, so the joined terminal ends the flow immediately, resetting the client
  where a failure or a report is what ended it.

Entering that retained phase discards nothing by itself. What can still discard is a later abortive ending:
the outer idle floor, a configuration retirement that applies to the flow, or session shutdown, each of which
reclaims the flow along with whatever the stream still held. So it is reclaimed when its client side reaches
`Closed`, when its floor passes, when a retirement applies, or when the session ends - whichever comes first,
and exactly once.

The join fence applies wherever there is something to join: a flow with a running task is cancelled and
joined before its record is reclaimed and its reservation refunded, while a flow closing client-side was
already joined at its own terminal and needs no second join - a retirement settles it directly.

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
  and drop its own independent TUN descriptor, but nothing signals a wedged one - the SIGTERM/SIGKILL
  escalation that would do so belongs to the ordered stop, which is exactly what did not run - so both the
  child and that descriptor can survive the app. The global `preferTestNetworks` flag is likewise left set.
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
  permissions or hidden-API access; Shizuku being superseded while the permission request is outstanding;
  tethering-service death, or a tethering-callback failure, before the first upstream observation; native
  launch, authentication or descriptor-transfer failure; TUN, request, agent, preference, publication or
  security-readback failure; the exact request expiring before ConnectivityService publishes the native
  network, which is the only announcement a failed native creation has; agent or request loss before commit;
  and a foreign TestNetwork collision.
  Rollback usually leaves nothing behind, but a failure around an ambiguous Binder publication can end in one
  of the unknown or residual outcomes tabulated above.
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
| Failure injection, partial | Killing the app-UID daemon while the app stayed alive produced prompt network removal and session retirement rather than a session left believing it still owned a dataplane. Force-stopping the app removed the network with the process, but ran no finalizer, so `preferTestNetworks` was left set | rooted with root-backed Shizuku, USB tethering |
| Cleanup | Ordered cleanup on normal stop, leaving no network, request, TUN, child or foreground service | rooted with root-backed Shizuku, and stock non-rooted with shell-backed Shizuku |
| Security boundary | Restricted `Network`-handle selection is enforced against a separately signed app and against the owner UID alike; direct interface injection succeeds. Isolation was tested and **failed** | separately signed out-of-repository attacker harness plus TUN packet capture, distinct from the rows above and proving nothing about release behaviour |

## Related Code And Documentation

- [`mobile/src/main/java/be/mygod/vpnhotspot/shizuku/`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/) -
  session ownership, ordering, the Shizuku identity, the privileged manager, the agent, the pinned tethering
  connector and the child conversation;
  [`ShizukuTetheringService.kt`](../../mobile/src/main/java/be/mygod/vpnhotspot/ShizukuTetheringService.kt)
  owns the job and the process lifetime behind it.
- [`mobile/src/main/rust/vpnhotspotd/src/shizuku/`](../../mobile/src/main/rust/vpnhotspotd/src/shizuku/) - the app-UID
  dataplane; see the source map in [`README.md`](README.md).
- [`routing.md`](routing.md#rootless-shizuku-mode) - the external-state and cleanup catalog;
  [`lifecycle.md`](lifecycle.md#app-uid-session-start) - the start call and
  [`#selected-network-handover`](lifecycle.md#selected-network-handover) - handover;
  [`dns.md`](dns.md), [`errors.md`](errors.md), [`traffic.md`](traffic.md) and
  [`invariants.md`](invariants.md) - resolver handoff, reports, accounting scope and cross-module rules.
- [`daemon.proto`](../../mobile/src/main/proto/daemon.proto) - the wire schema, including the app-UID
  command family and `ShizukuSessionConfig`;
  [`mobile/src/hiddenApiStubs/README.md`](../../mobile/src/hiddenApiStubs/README.md) and the root
  [`README.md`](../../README.md) - API inventory and compatibility assumptions.
