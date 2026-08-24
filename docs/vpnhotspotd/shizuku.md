# Shizuku Mode

Shizuku mode shares this app's own default connection with tethered clients without root. It publishes a
restricted `TRANSPORT_TEST` network over a TUN it owns, lets Android's system tethering select that network
as its upstream, and relays the resulting traffic from an app-UID child process onto whatever `Network`
Android has made this app's default - a VPN when one applies to this UID, and the ordinary default when none
does.

It is **one global upstream mode**. It owns no downstream: it never reads, stores, logs or makes policy from
`ncm0`, `wlan2`, or any other tethered interface name, and it never starts, stops or cycles Android system
tethering. Everything it publishes is a property of the session's own TUN and of the exact `Network` the
agent registered.

Root mode and this mode are independent in lifecycle, control-plane state and resource ownership: neither
consults, claims, delays or restores the other, and both may run at once. `RoutingManager`, `Routing` and
`TetheringService` are unchanged by this mode.

Packet routing is where they meet, and only the kernel arbitrates it: root's per-interface policy rules sit
at the priorities in [`routing.md`](routing.md#priority-and-table-model), all numerically lower than the
tethering upstream rule Android installs for the selected network, and Linux evaluates the lower priority
first - so root's routing takes precedence for the downstreams it carries, with neither side told anything.

Shizuku's shell or root identity is used **only** for Android control operations. The dataplane child and
every egress socket run under the app UID, so the whole design is shaped by having no privileged dataplane
available at all.

Requires Android 13 (API 33) or later.

## Scope And Limitations

These are deliberate scope limitations, not defects awaiting a fix. Root mode has none of them, which is why it remains
the recommended path.

- **No isolation from other apps on the device.** The restricted network denies `Network`-handle selection,
  but any UID that can name the interface can put packets on it. See [Security Boundary](#security-boundary).
- **No client identity.** Android's inner IPv4 NAT translates client addresses, ports and Echo identifiers
  before packets reach the TUN, and any local app can forge any source. There is no per-client blocking, no
  per-client accounting, and no trustworthy physical-client identity of any kind.
- **A bounded set of supported traffic.** TCP, UDP, DNS terminated at the virtual resolver addresses, ICMP
  Echo, and a bounded set of translated ICMP errors. ESP, GRE, SCTP, unknown IP protocols and downstream
  link control (RA/RS/ND, DHCP, ARP) are not carried; Android tethering owns downstream link control.
- **IPv6 reaches at most one downstream.** Tethering passes an upstream `/64` only to the oldest active
  downstream
  ([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#201)),
  so a second tethered interface is IPv4-only - and *at most*, because a local-only downstream can hold that
  position while being answered from an earlier branch that never consults the upstream
  ([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#191)),
  leaving every tethered interface without IPv6. This app's own local-only hotspot can produce that ordering
  exactly like any other app's, since it runs independently of this mode.
- **Selection is not commanded.** `setPreferTestNetworks` is a preference, not a reselection trigger. See
  [System Tethering Selection](#system-tethering-selection).
- **Losing the network does not stop tethering.** Android reselects an ordinary upstream and clients keep
  working, unprotected, with nothing on their side changing to say so.
- **One TestNetwork controller.** A foreign `TRANSPORT_TEST` network holding the tethering upstream is a
  terminal collision, not a state to cycle out of.

## Ownership

| Owner | Owns | Does not own |
| --- | --- | --- |
| [`ShizukuTestNetwork`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/ShizukuTestNetwork.kt) | the one session generation: TUN descriptor, exact request, agent, pinned tethering connector, global preference, child, upstream observation | anything of root mode's; any downstream |
| [`ShizukuLifecycle`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/ShizukuLifecycle.kt) | command serialization and publication rollback | the resources themselves |
| [`ShizukuTetheringService`](../../mobile/src/main/java/be/mygod/vpnhotspot/ShizukuTetheringService.kt) | process lifetime and the one ordered command path | session state; the shared notification's text |
| [`AppUidDaemon`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/AppUidDaemon.kt) | the launched child, its authentication, and the level-triggered config conversation | the dataplane |
| The daemon's ingress task | every piece of client-keyed dataplane state | the platform's resolver work once submitted |

Three rules hold the Kotlin side together.

**One session generation at a time.** `ShizukuTestNetwork` publishes exactly one generation, confined to a
single-lane dispatcher, and a successor is admitted only when that generation reaches its final committed
transition. Each resource is a
[`SessionResource`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/SessionResource.kt) with named
states rather than a nullable field, because a resource whose outcome is *unknown* is not the same as one
that was never created and a withdrawal has to act differently on each. What evidence decides which is
per resource - see [Startup And Commit](#startup-and-commit).

**One command lane.** `ShizukuLifecycle` runs one start or stop at a time. A duplicate press shares the
start already in flight; a stop issued during publication queues behind it; a stop that arrives while a
start is still in its interactive half - Shizuku authorization, which can sit on the user's own permission
dialog - supersedes it instead, because nothing has been created yet. A supersession keeps reporting as a
stop in progress until the superseded start has unwound, since until then that start still owns the flight a
duplicate press would share. From the moment publication begins the start either completes or rolls back.

**One rollback owner.** A publication that fails throws with its ledger intact; the lane runs the single
retirement over it. The publication does not also unwind itself, because two owners would mean a failed
rollback was retried immediately by the start that caused it instead of being left for the next explicit
stop.

The app's row is status-only. It shows the current state and its own switch, exactly like every other row on
that screen, and carries no instructions.

## Platform Integration

### Privileged Shizuku Operations

Every Android control operation runs inside a Shizuku *epoch*: an authorized identity, its wrapped binders,
and the effective UID they act as.
[`ShizukuEpoch`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/ShizukuEpoch.kt) brackets each
transaction so that a replaced or dead identity is caught before a result is believed, and it checks the
epoch again when a success could confirm a mutation that already happened. Nothing is confirmed on a closing
check that failed.

Effective-UID equality, not Binder continuity, is what makes a later release legal. ConnectivityService
authorizes `releaseNetworkRequest` against the UID stored with the request, asynchronously, so a call under
the wrong UID would no-op while the app-facing wrapper still cleared this process's own bookkeeping - a
false confirmation. A cleanup-only epoch is therefore required to have the same effective UID as the session
that issued the request, and the direct service call on the retained handle is used rather than
`unregisterNetworkCallback`, which would destroy the only handle a retry has.

The privileged `ConnectivityManager` is built without running a constructor, so the process-wide singleton
is never written and ordinary `Context` lookups keep returning the ordinary manager. It is reachable only
through a private context that no app code holds. The exact hidden and system API inventory lives in
[`mobile/src/hiddenApiStubs/README.md`](../../mobile/src/hiddenApiStubs/README.md) and the user-facing
compatibility assumptions in the root [`README.md`](../../README.md); neither is repeated here.

### Restricted TestNetwork

The implementation calls `TestNetworkManager.createTunInterface` and **never** `setupTestNetwork`. AOSP's
setup path adds `NET_CAPABILITY_NOT_RESTRICTED`, which would let any installed app select the network.

Publication is this app's own `NetworkAgent`. The constructed capabilities carry more than the list below -
builder defaults and several convenience capabilities are kept - so this is the security-relevant subset,
not the whole set, which lives in `ShizukuTestNetwork.publish`:

- `TRANSPORT_TEST`, with the session's own `TestNetworkSpecifier` naming its `testtunN`;
- `NET_CAPABILITY_NOT_RESTRICTED` and `NET_CAPABILITY_TRUSTED` removed from the builder's defaults, and
  `NET_CAPABILITY_INTERNET` never added. The first is what would make the network usable by ordinary apps;
- an empty allowed-UID set, which is a fresh builder's default and is never assigned;
- legacy type `TYPE_TEST` and score 1, so the retained exact request is what keeps the agent wanted.

The specifier comes from the deprecated `String` overload on purpose: it yields a `TestNetworkSpecifier`
only because `TRANSPORT_TEST` is already set on the builder, and asserting the resulting class turns a
silent `EthernetNetworkSpecifier` mismatch into an explicit startup failure rather than a publication
timeout.

The interface contract is fixed for the life of a session and the daemon is pinned to it:

| | Value |
| --- | --- |
| IPv4 | `192.0.2.1/30` |
| IPv6 | `2001:db8:1::1/64` |
| Virtual DNS | `192.0.2.2`, `fd00::53` |
| MTU | 1500 |

They are documentation prefixes because every address the interface holds is an address clients cannot
reach, and [`routing.md`](routing.md#rootless-shizuku-mode) says why. The IPv6 one additionally has to
*look* global. Tethering copies only link addresses that are
`isGlobalPreferred()` with a prefix length of exactly 64 from its upstream into the downstream config
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#239)),
and that predicate rejects ULAs by name
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/LinkAddress.java#487)).
The deterministic `fd...` prefix root mode uses would therefore yield no client IPv6 at all. The same file
admits ULAs as *DNS servers* through a looser predicate, which is why `fd00::53` is fine as a resolver
address and would not be fine as the prefix.

`LinkProperties` is built once and never mutated, and its contents are catalogued with the rest. Two things
about it are this mode's own: the addresses are re-read from the kernel so the published set matches what
`createTunInterface` really assigned rather than what was asked for, and the IPv6 default route is there
because tethering requires one alongside the global `/64` before it delegates - the upstream DNS list is not
among the conditions
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#210)).
ConnectivityService adds connected routes during registration, so they are precomputed and required back in
the readback.

### System Tethering Selection

This mode sets one global flag and observes the result. It sends
`ITetheringConnector.setPreferTestNetworks(true)` through a pinned connector rather than
`TetheringManager.setPreferTestNetworks`, because the manager discards the result code
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#2241))
and the result code is required: without `NETWORK_SETTINGS` the service reports
`TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION` through the listener instead of throwing, so a denial is
otherwise silent. The interface is `oneway`, so the transact returns before the service acts and the answer
arrives separately under a deadline. `IBinder` guarantees ordering only for one-way calls to the *same*
object, which is why both writes go to the one pinned connector of one epoch and why cleanup carries a
pending clear rather than offloading one to a replacement epoch's connector, where the two could be applied
in the wrong order and strand the flag.

**Success only means the flag moved.** It is not a reselection trigger and it is never proof that tethering
selected this network. Tethering that already holds an ordinary upstream stays there until Android
reevaluates for its own reasons, or until tethering is cycled by something outside this mode. That is the
ordinary case rather than an error, and it is what `RESTART_REQUIRED` reports.

On Android 13 the preference is consulted only when automatic upstream selection is on
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/src/com/android/networkstack/tethering/Tethering.java#1798)),
so a build that disables it can never run this mode and is detected before anything is created. From Android
14 the tethering module forces automatic mode on, so there is nothing to check.

Two platform facts shape what the daemon sees once selection happens.

**Double NAT.** Android's own forwarding and MASQUERADE run before packets reach the TUN, so Android owns
the inner mapping, filtering, reverse conntrack and timeouts while the daemon owns only its outer
selected-network state, and never mirrors or refreshes Android's.

**Proxied DNS.** Android may originate forwarded DNS from a TestNetwork-local address, especially for IPv6
RDNSS, so client DNS arrives at the virtual resolver addresses from the platform rather than from the
client.

The tethering service's own death is terminal for this process: `TetheringManager` caches its connector
permanently and AOSP states that no recovery is possible
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#467)),
so a session ends and a further one is refused with that in so many words rather than left to fail
obscurely. Its death also proves the preference was reset by the process that held it, so the clear this
session owed is discharged rather than retried against a service that has forgotten it.

## Session State And Lifecycle

### State And Admission

The session state is recomputed from the global upstream observation for as long as the session runs.
**Only `ACTIVE` admits dataplane traffic** - as the rule the applied config carries, not as an instantaneous
guarantee read off the displayed state. What the daemon admits is whatever the last acknowledged config
said, so the two agree once that config lands.

| State | Meaning | Admits |
| --- | --- | --- |
| `ARMED` | Tethering names no upstream, so there is nothing to carry | no |
| `VERIFYING` | Tethering names an upstream this session cannot currently classify | no |
| `ACTIVE` | Tethering reports the exact `Network` this session published | **yes** |
| `RESTART_REQUIRED` | Tethering reports an ordinary upstream | no |
| `STOPPING` | Withdrawal published and the ordered stop is running | not once its `closeAdmission` is acknowledged or the child is fenced; the previously applied config may still admit until then |

Ownership is decided by identity against the exact `Network` the agent returned, never by a capability read.
That is positive proof: the registered agent returned it, the exact request whose specifier is this
session's own `testtunN` resolved to the same value, and the agent stays registered for as long as the
comparison is made, so the netId cannot have been reissued. Classifying a *non*-owned upstream does need the
platform, and needs no privilege: `getNetworkCapabilities` enforces `ACCESS_NETWORK_STATE` alone and its
sanitizer never redacts transports, so `TRANSPORT_TEST` is readable for a live network this app cannot use.
A confirmed non-TEST upstream is `RESTART_REQUIRED`; a `TRANSPORT_TEST` one is the terminal collision; a
network that disappeared between tethering naming it and the read stays `VERIFYING`.

The command lane publishes its own coarser state - preparing, publishing, on, retiring - which is what the
row renders as busy. It exists because the authorization and startup window has no session state yet, and
rendering that window as off is what would let the row accept a second start.

### Startup And Commit

Startup has two halves. The first creates no session resource and stays cancellable while it waits on the
user's authorization; the second creates things and either completes or rolls back. Preparation is not
mutation-free, though: settling what a previous session in this process still owed can release a privileged
request or clear the global preference.

Preparation, in order:

1. Authorize a Shizuku epoch, and on Android 13 require automatic upstream selection. A device that would
   never consult the preference is refused here rather than left in a permanent `RESTART_REQUIRED`.
2. Finish whatever a previous session in this process still owed. A fresh epoch is the one thing an
   outstanding privileged release was missing, so the retry belongs here; a session that still cannot confirm
   it refuses the start.
3. Refuse to start if any `TRANSPORT_TEST` network already exists. A newly started app process has no
   in-memory generation, so a *published* test network that outlived one is only detectable by asking the
   platform - the scan proves that and nothing more, since a TUN with no agent over it is invisible to it -
   and asking before this session registers an agent is what keeps it from matching itself.

Publication, in order:

4. Resolve the reflective members a *retirement* needs - the direct request release, the callback's exact
   request, and the child's pid accessor. Discovering one of them unreachable after the TUN, the request, the
   preference and the agent exist would leave a session that cannot be taken back.
5. Build the private manager and the `TestNetworkManager`, then create the TUN. The descriptor is recorded
   live inside the epoch bracket, before its closing check: `createTunInterface` hands it back by return
   value, so either this process received it and owns it or the reply was lost and there is nothing to own.
6. Register the upstream and egress observations, and wait for the first upstream value. This precedes the
   preference and the agent - the only two mutations that can move tethering's upstream - so the first
   snapshot predates everything that matters. No selectable *egress* is not waited for: that is a steady
   state, not a startup value still to arrive.
7. Spawn the child and complete the bootstrap handshake. Authentication comes first - peer UID and PID, then
   the nonce - and only an authenticated peer is sent the config frame the duplicate TUN descriptor rides on.
   Ownership still begins at spawn rather than at a completed handshake, because the process exists from
   then on and a failure after that transfer, or while readiness is being confirmed, can leave the child
   holding the duplicate. Mechanics are in [`lifecycle.md`](lifecycle.md#app-uid-bootstrap).
8. Register the exact foreground request, then acquire the pinned connector and link its death recipient,
   then set the preference - in that order, because connector death is what silently undoes the preference.
9. Register and connect the agent, then wait for both `onNetworkCreated` and the request's `onAvailable` for
   the same `Network`, and validate the capabilities and `LinkProperties` that came back.
10. Commit a state, send the first config, and install the one watcher that ends a committed session. The
    watcher is installed only after the first config, so until then nothing else can be withdrawing.

The session ledger records ownership, release debt and *uncertainty*, each under the evidence its own
resource can actually produce. Three resources are acquired by return value, so ownership begins only when
the call hands one back: the descriptor when `createTunInterface` returns it, the child when spawn returns,
and the connector when acquisition returns it.

The other three are ambiguous IPC publications, recorded as owed from the moment the transaction is
*issued*, because for these a thrown exception is not proof that nothing happened: the exact request before
`requestNetwork`, the global preference before the one-way `setPreferTestNetworks`, and the agent before
`register()`. They are then classified by evidence rather than by the shape of a failure - the request and
the agent by the exact handle and `Network` the framework writes back, and the preference by the service's
own result code, which is the only thing that separates "did not act" from "may have acted".

`UNKNOWN` is a state rather than a gap: it means existence is not known, and it is what the withdrawal keys
its fail-closed behaviour on. A failure anywhere therefore leaves a ledger naming what is owned, what is
owed and what cannot be decided - not a claim that it names exactly what exists.

### Upstream Generation And Downstream Epoch

Two independent axes travel in every config, and the daemon refuses one whose axes disagree with the fields
they retire.

**`upstream_generation`** says which `Network` the egress sockets are bound to. It advances on every change
to the selected `Upstream` value - including a `LinkProperties` change that leaves the raw handle equal,
because the state pinned behind that `Network` is stale just the same - and it is deliberately not the
handle itself, since a handle is derived from a netId that is eventually reused. The interface index is
resolved from the selected interface name while each config is built, so it can move with no observation
behind it; the publication owner notices that and advances the generation for it, coalesced so one logical
change never advances twice. The sweep this drives is in
[`lifecycle.md`](lifecycle.md#selected-network-handover).

**`downstream_epoch`** says whether a TUN-visible tuple still names the same client, and everything keyed by
such a tuple carries it: TCP flows, UDP mappings, Echo sessions, reassembly contexts and virtual-DNS
transports. It advances on any loss of positive confirmation that tethering is still carrying this exact
network - a non-session upstream, no upstream at all, or an unclassifiable one. Because this mode is global
rather than per-downstream, that is the only thing that moves it: tethering can rebuild its NAT behind an
unchanged `Network` handle, so continuity has to be established rather than assumed from a short absence.

Egress is the app UID's own default `Network` and nothing else. Root mode's upstream preferences take no
part in it, the session's own TUN is rejected by interface name so the daemon cannot relay to itself, and
**there is no fallback network**: no selectable `Network` is a legitimate steady state in which upstream
work fails per operation and the session resumes on the next selection.

### Stop, Rollback, Child And Binder Death

A normal stop is ordered, and the order is the platform's rather than a preference.

1. Install the session's retirement deferred before the first suspension - that, not the published state, is
   what makes two observations deciding the session is over produce one withdrawal - then publish `STOPPING`
   as the controller state behind it, and cancel and *join* the session's own observers. Only then ask the
   daemon to close admission and await its acknowledgement: until that lands, the config it last applied is
   still in force. A daemon that cannot acknowledge is not left admitting through the next step's
   minute-long deadline - the child fence runs immediately instead.
2. Clear `preferTestNetworks` through the still-live connector, then unlink its death recipient. This is the
   one piece of system state that outlives a session, so it is dropped while there is still a connector to
   drop it through.
3. Fence the child: close the socket, wait for exit, `destroy()` for SIGTERM, wait again, then SIGKILL the
   launched child PID - validated against peer credentials during authentication - explicitly, and wait for
   observed exit. `destroyForcibly()` is not an escalation on
   Android - it calls `destroy()`, whose native side is already `kill(pid, SIGTERM)`
   ([source](https://android.googlesource.com/platform/libcore/+/refs/tags/android-17.0.0_r1/ojluni/src/main/native/UNIXProcess_md.c#1056))
   - so the explicit kill is what handles a wedged child. Everything downstream assumes the child is gone.
4. Withdraw the agent, then prove the network is gone before releasing the request. ConnectivityService
   emits the request's loss when the agent disconnects, before it removes and rematches the network, so
   unregistering the callback first would make framework dispatch drop the queued loss. The barrier that is
   always owed is `onNetworkUnwanted`, delivered from `NetworkAgentInfo.disconnect()` whether or not a
   native network was ever created; `onNetworkDestroyed` is additionally required once `onNetworkCreated`
   arrived, and the request's `onLost` once it reached `onAvailable`. Agent unregistration is deliberately
   not bracketed in the epoch: it reaches ConnectivityService through this process's own registry handle, so
   it still works after Shizuku death.
5. Close the descriptor. Only now, and never at the top: a successor inheriting it would be relaying through
   a TUN ConnectivityService still exposes.
6. Release the exact request on the retained handle, then clean the framework's own callback bookkeeping.

A withdrawal reports itself finished only once every *local* resource is fenced, and locally unfenced means
the descriptor, the child or the agent - not the request. The three unknown outcomes are therefore not the
same failure.

- **An unknown agent is locally unfenced.** The absence of `onNetworkCreated` is not proof that no remote
  agent exists, and the unregister transition acts only on a live one, so no later stop can recover it and
  no retry would learn more. The mode keeps reading as on, because a native network this process may still
  own and cannot name is exactly what must not be reported as gone; process death is the recovery.
- **An unknown request is unreleasable residual debt, and implies no native network.** The release path
  refuses to act on it, so local resources still retire completely and the session settles as residual. What
  it costs is a successor: no further session runs in this process until the process ends.
- **A privileged release that was merely unconfirmed is the recoverable case.** The session is over, a
  successor is refused meanwhile, and a later start retries it before creating anything.

Rollback is the same ordered withdrawal, idempotent and resumable: the first caller runs the steps and the
rest await it, each step confirms its own ledger entry so a retry does what is left, and the whole of it is
non-cancellable because abandoning mid-step would leave exactly the state it exists to remove.

A committed session also ends without being asked, and one watcher owns all of it. It selects on the
tethering connector's death, the daemon control conversation ending, the agent's destruction, the exact
request's loss, and the session's own reported failure; the first to fire is reported to the user and hands
the withdrawal to a scope the withdrawal cannot cancel. The daemon-ended arm is what makes child death
prompt: a config round trip races it rather than waiting out its acknowledgement deadline, so a child that
is simply gone is discovered at once instead of a minute later.

**Process death.** Nothing waits for anything. The agent, the request and the native network go with the
process that hosted them. The child is not bound to its parent by the platform: what ends it is EOF on the
control socket the dying process closes, after which it exits and drops its own copy of the TUN descriptor -
the ordered stop additionally fences it with TERM then KILL, which process death cannot. Of the *app-owned*
mutations the global preference is the one that can be stranded; the framework-owned netd counter rules
catalogued alongside it may also remain, but were never a resource this process could release. Both are in
[External State And Cleanup](#external-state-and-cleanup). A component teardown this mode did not ask for
runs its withdrawal in a scope outliving the service, which is the abnormal path the design accepts:
foreground importance cannot be kept past `onDestroy`.

## App-UID Dataplane

The child is the same `vpnhotspotd` binary the root mode uses, exec'd in place from the APK with the app
UID. It speaks a level-triggered config loop rather than the root call/reply conversation: the newest config
is the whole truth, and each is answered with what was actually applied only after the ingress task confirms
that the state *the changed axes require to retire* is gone rather than merely asked to go. That is narrower
than "nothing from before it remains": a resolver transaction is never cancelled to free capacity, so it
outlives the config that retired whatever asked for it, keeping either its descriptor and token or - once
this process can no longer observe it - only the quarantined token. See
[`lifecycle.md`](lifecycle.md#app-uid-bootstrap).

Every packet read from the TUN is untrusted input from an unknown local principal. Destination is compared
against the session's exact virtual-address set before attribution, reassembly or transport dispatch,
because those addresses are ones the daemon answers for rather than relays. The three principals - DNS,
IPv4, IPv6 - are shared classes, not identities; nothing derives identity from a source address.

### Supported Traffic

| Client traffic | How it is carried | Outer state the daemon owns |
| --- | --- | --- |
| TCP | terminated locally by `smoltcp` and reconnected upstream, so each side segments to its own MTU | flow record, socket, two 64 KiB buffers, one upstream descriptor |
| UDP | relayed through one endpoint-independent, address-filtered mapping per TUN-visible source, on an unconnected socket reused across destinations | mapping, remote records, bounded send history |
| DNS to a virtual resolver address | terminated and handed to the platform resolver, over UDP and over TCP | transaction row, logical resolver token |
| ICMP Echo | relayed on a ping socket | Echo session and socket |
| ICMP errors | translated only where the daemon can prove what they describe | none |
| Anything else | dropped | none |

Those UDP semantics are the outer ones only: Android's inner NAT is in front of them and this daemon does
not mirror it, so nothing here promises end-to-end mapping or filtering behaviour for a physical client.

Three drop classes are distinguished because they mean different things: malformed or truncated packets, an
unsupported protocol or port aimed at an address the daemon occupies, and a destination that means something
only on the link it arrived from - multicast, broadcast, link-local, loopback, unspecified. Private and
unique-local addresses are deliberately *not* in the last class: they are ordinary destinations for a VPN or
a NATted upstream, and the resolver this daemon relays to is usually one of them.

An ICMP error is a third party's claim about a packet the daemon sent, so it is repeated to a client only
when the proof behind it is no narrower than the claim: an error about one datagram needs that datagram
identified, and address-level proof alone carries only route-level claims. The translation rules themselves
live with the code, in `shared/icmp_translate.rs`. [`errors.md`](errors.md) owns the daemon's *report* and
terminal-error semantics, which is a different subject from these client-facing packets, and
[`dns.md`](dns.md) owns the resolver handoff.

### MTU, Output, And Fragments

MTU 1500 is immutable in the agent's `LinkProperties` and in the daemon. Tethering clamps the downstream
IPv6 MTU it derives from its upstream to 1280-1500, so 1500 is the top of what the platform will propagate.

The DF floor the daemon sizes relayed IPv4 output against is that same fixed session MTU, sent as
`downstream_mtu_floor`. This mode owns no downstream, so there is nothing narrower to measure and nothing
that could move it within a session. The cost is the one case where a downstream link is narrower: a
DF-set packet above that link's MTU is dropped by Android's forwarding path, which answers ICMP Fragmentation
Needed back at the TUN - the signal this design already derives path-MTU behaviour from - so the failure is
a signalled path-MTU event at the cost of one packet and a round trip rather than a black hole. The field
remains axis-guarded: it may only change together with `downstream_epoch`, because it is what every
already-queued packet was sized against.

The selected network is usually smaller than the TUN, so path-MTU signalling toward clients is load-bearing
rather than optional: it comes from `EMSGSIZE` on a DF-set send and from the error queues, never from a
configured upstream MTU a handover can change.

In this mode the floor and the TUN's own MTU are the same 1500, so there are only two cases. A datagram
within the floor goes out whole - IPv4 with DF set, IPv6 with no Fragment header - and neither needs an
Identification guard. A datagram above it cannot be carried whole, as a UDP reply to a client's own query
can be several kilobytes, so both families source-fragment, IPv4 reusing the datagram's one guarded
Identification across its fragments.

Newly originated TUN-side packets use hop limit 64. Relayed traffic and translated errors preserve validated
received hop metadata and never substitute that local default.

All TUN writes pass through one packet writer with bounded queueing, atomic packet writes, final size
validation and source fragmentation. The size policy, the fragment-identifier decision and the splitting all
happen in the ingress task's output owner *before* anything is queued; the writer dequeues, checks the
retirement stamp, validates the finished bytes and writes.

That owner holds **two** session-wide fragment-identifier mechanisms: a per-tuple guarded Identification
table for IPv4, and beside it one wrapping 32-bit sequence for IPv6 fragment headers. Both are shared across
producers for the same reason - ports are not in the tuple a receiver reassembles on, so per-producer
allocators would let a UDP reply and a DNS answer to one client mis-splice - but only the IPv4 one is
guarded.

Two things are easy to conflate and must not be. The daemon's own queue filling is an admission decision;
the kernel's TUN queue filling is `EAGAIN` on a nonblocking descriptor, which is a wait for writability and
never re-admits or re-charges a packet already accepted. And a "partial write" can only mean a partial
*datagram*: a TUN descriptor delivers one write as one packet or fails, so the only partial case is a
multi-fragment datagram whose later fragments fail, which never replays the datagram or synthesizes an error
for the unsent remainder.

The receiver-facing invariant below is **IPv4's alone**, and it is about *datagrams* rather than packets:
every fragment of one fragmented IPv4 datagram deliberately carries the same
`(source, destination, protocol, Identification)`, which is what makes them reassemble into one datagram
instead of several. What must not happen is a *different* guarded datagram reusing that identity until 60
seconds after the previous datagram's latest successful fragment write. Within-floor output is outside it in
both families - a DF-set IPv4 packet and an unfragmented IPv6 one are never reassembled and are issued no
fragment identifier - and oversized IPv6 takes its Fragment header value from the shared wrapping sequence,
which carries none of the rules below.

Three rules make the IPv4 guard hold. A tuple's sequence *ends* after the whole 16-bit space rather than
wrapping, and
restarts only once no packet of it is still queued and the window has elapsed; until then a datagram is
denied and counted. The clock runs from the writer's own successful write rather than from allocation, so a
stale dequeue, a validation refusal and a retirement-preempted blocked write are endings that were never
written and start no window. And every new session denies guarded output for its first 60 seconds, which is
the only thing covering what a predecessor wrote a second before it stopped.

Ingress reassembly is bounded in both families by overlap, extension, length and timeout rules: overlapping
or inconsistent fragments are rejected, and IPv6 extension-header parsing is bounded and refuses forbidden
chains.

### Resource Ownership And Admission

One owner decides whether traffic-driven state may exist, in **two currencies that do not substitute for one
another**. A descriptor is not a byte: a UDP mapping costs one descriptor and a few hundred bytes while a
terminated TCP flow costs one descriptor and two 64 KiB buffers, so counting the second as "one record"
would let memory run out long before descriptors do, and counting the first in bytes would let forged
sources exhaust the process's descriptors while the byte total still said there was room. Every reserve
names both, and a request that fits one but not the other is denied.

Both totals are measured rather than chosen. The descriptor total comes from `RLIMIT_NOFILE` and
`/proc/self/fd`; the byte total is a conservative eighth of the kernel's own `MemAvailable` estimate at
session start. That byte total is a policy *share*, not a process ceiling and not a promise that exceeding
it would fail: it counts Rust-visible owned heap this daemon chooses the size of, and deliberately excludes
allocator-private metadata and a hash container's own indexing, for which row *count* is enforced instead.
Capacity, not length, is the unit, so expiring an entry refunds its record and frees its logical slot but
refunds bytes only when the owner says an allocation really went.

Two floors sit *inside* their totals rather than beside them: descriptors held back for DNS, and bytes held
back for essential work. General traffic cannot reach into them; DNS-class and essential-class work can. The
DNS floor is sized by the daemon's own nested resolver ceiling, an eighth of the platform's 256-query
per-UID limit, because the daemon is not the only holder of those slots - the platform's own proxy, a
replaced session's draining queries and the app process itself share them. Exhaustion answers SERVFAIL
rather than being treated as an invariant violation: sizing makes it rare, handling makes it harmless.

Three invariants hold the accounting together.

- **Deny new, never evict live.** A denial is a denial. What to do about one - process due expiry, retire
  the requester's own oldest optional history, retire the globally oldest, ask the fragment owner to drop
  the requester's oldest non-reassembly context, and only then give up - is an ordering the ingress owner
  knows. Established transport state is never evicted to admit new work, at any step.
- **Cancel, join, close, then refund.** A grant is an inert lease that never refunds itself: not on
  cancellation, not in a `Drop`, not on an error path, because every one of those runs before the thing
  being accounted for is actually gone. An owner cancels, waits for the task to run to completion, takes
  back its record, and refunds only then - so an acknowledged config means the descriptors of everything it
  retired really are back. A resolver transaction it deliberately did not retire is outside that: it still
  holds its own descriptor while it stays observable, and holds only a quarantined token once it does not
  ([`lifecycle.md`](lifecycle.md#app-uid-bootstrap)). A lease dropped without release leaks for the rest of
  the session and shows up as an outstanding entry, which is fail-closed rather than fail-open.
- **One reservation per row, taken before the payload.** A reply task takes its queue slot before it
  allocates, which is what makes a bounded queue a real bound rather than a nominal one.

Because there is no client identity, budgets are self-protection rather than fairness.

### Idle Floors And Timers

These govern only the daemon's own outer state. Android's inner NAT keeps conntrack state for the same
client, and none of it is mirrored, configured or timed from here.

| State | Floor |
| --- | --- |
| UDP mapping and remote records | 300 s idle |
| UDP error history record | 60 s absolute; never refreshed |
| TCP established, and the half-closed phases that can still carry data | 7,440 s idle |
| TCP before a connection is made, and once neither direction has anything left to carry | 240 s idle |
| Echo session | 60 s idle |
| IPv4/IPv6 incomplete reassembly | 60 s |
| IPv4 Identification nonreuse | 60 s from the write |

Outbound UDP activity refreshes only its own mapping and the relevant remote. Inbound packets, rejected
packets and ICMP errors do not. Once the stop's `closeAdmission` is acknowledged - or the child is fenced
because it could not be - the session may use existing state but creates, tracks and refreshes nothing;
until then the daemon is still running the previous admitting config. Either way stopping is not pausing:
the deadlines keep running and what they retire is still taken back.

#### A Flow Can Outlive Its Worker

A TCP worker returns as soon as *its* ordered work is done: the upstream half-close is written, the remote's
end of stream has been handed over, and the client's stack has taken it. The client's own teardown is not
finished at that moment - the socket is typically in `LAST-ACK`, `CLOSING` or `TIME-WAIT`, with a FIN to
retransmit and a final acknowledgement to wait for. So a clean terminal from a flow nobody asked to stop
**detaches** the flow rather than ending it:

- the worker's own state is already gone, because its task ran to completion and the upstream descriptor
  went with it;
- the flow keeps its socket, both stack buffers, its conservative connection charge and its DNS state until
  `smoltcp` reaches `Closed`, its outer floor runs out, a config retires it, or the session ends;
- nothing stands behind it - no task of its own and no per-flow timer task. Its teardown is still scheduled,
  by the combined `smoltcp`-and-outer deadline the ingress owner already sleeps on, which is what lets a FIN
  be retransmitted. That owner scans its own rows for a detached flow whose socket has closed, exactly as it
  scans for a settled resolver transaction, and that scan is what settles it.

Two endings are excluded because there is no teardown to protect: a *cancelled* worker also reports a clean
terminal, and there the socket has already been aborted by whoever cancelled it; and a socket that never got
past its handshake has no connection whose closing could be cut short. A worker that *failed* is not a clean
completion either - it resets its client and ends its flow at once.

Config retirement and session shutdown recognise a detached flow and settle it directly. Waiting for a
second worker terminal would be waiting for one that can never arrive.

#### Outer TCP Phases

The two TCP rows above are RFC 5382 section 5 REQ-5's floors, and which phase gets which is REQ-5's own
classification rather than a reading of the state names. The daemon keys it on the actual post-action
`smoltcp::socket::tcp::State`:

| `State` | Outer idle floor |
| --- | --- |
| `Listen`, `SynSent`, `SynReceived` | 240 s |
| `Established`, `FinWait1`, `FinWait2`, `CloseWait` | 7,440 s |
| `Closing`, `LastAck` | 240 s |
| `TimeWait` | none; `smoltcp`'s own `CLOSE_DELAY` owns it - ten seconds in the pinned 0.13.1, not a host's 2MSL |
| `Closed` | terminal |

`FinWait1`, `FinWait2` and `CloseWait` stay on the established floor because in each of them one direction
can still carry application data. Treating every FIN-looking state as transitory would reset a client four
minutes into a half-close it is entitled to hold open.

**No post-RST retention is claimed.** RFC 7857 recommends holding a mapping for four minutes after a
matching reset; this daemon does not. `Closed` is terminal, a reset from either side ends the flow, and a
tombstone would be new state created after the flow it describes is already gone.

These are idle floors, so matching activity rearms the whole of the current phase's floor rather than topping
it up. Two things rearm: a packet for the exact live tuple that was *offered* to `smoltcp`, judged from the
phase the socket is in after the poll; and a worker event on the exact current socket-and-worker pair that
really delivered payload or a real ordered end of stream. "Offered" is deliberately coarser than "accepted" -
the ingress parse reads the four-tuple, the hop limit and the SYN bit and nothing else, so a segment
`smoltcp` then discards for a bad checksum or an out-of-window sequence still rearms, because answering "did
the stack take this?" would mean a second TCP implementation beside the one the packet was just handed to.
What is refused *before* the stack sees it does not rearm, and nothing else rearms either: not stack output,
retransmissions or delayed acknowledgements, not a reset this daemon originated, not a config being applied,
and not while admission is closed.

Expiry runs on the ingress task's own wake, alongside `smoltcp`'s timers: the owner polls the stack first and
then applies whatever was due at the instant captured *before* that poll, which defers by one loop anything
that came due while the stack was running - the conservative direction, since a floor is a minimum. A due
flow is then retired in the engine's ordinary order, which is
[`lifecycle.md`](lifecycle.md#selected-network-handover)'s. Two differences from a handover are this owner's:
only the flow's own token is cancelled, so the abortive close a leaving network selects is not used; and a
reset is counted only where the stack has somewhere to send one, so an expiry can end a flow with no
client-visible packet at all. Expiring a DNS-over-TCP transport ends the transport only - its resolver
transaction keeps its row and its logical token until the platform is done with it, while an unobservable one
releases the ordinary row and descriptor and leaves only the quarantined token
([`lifecycle.md`](lifecycle.md#app-uid-bootstrap)).

## External State And Cleanup

This mode issues **no** root-daemon command, netlink write, netfilter rule, `ndc` request or sysctl change,
and it cannot: every one of those needs root, while both the app and its daemon run at the app UID. Root's
`CleanRoutingCommand` neither creates nor removes anything this mode owns, and it neither stops nor disturbs
a running session; equally, nothing here is a cross-mode action against root.

Issuing no command is not the same as causing no external state. Registering the agent materializes a native
network with routes and resolvers inside ConnectivityService, and Android tethering *selecting* that network
materializes forwarding, NAT, a delegated prefix and netd counter-chain entries against the session's
`testtunN` - none of which this mode requests, configures, or cycles tethering to obtain or release. Each
item's trigger, owner, teardown and process-death behaviour is catalogued in
[`routing.md`](routing.md#rootless-shizuku-mode). The feature-level invariants belong here.

**Every resource is token-owned except one.** The TUN, the agent and its native network, the exact request
and the child are held by a descriptor, a Binder or a process, so releasing them needs no private persistent
bookkeeping and no Clean path: the ordered stop, a rolled-back startup and the process ending each release
them without depending on a database, a preference or daemon memory surviving.

**The global `preferTestNetworks` flag is the carve-out**, an Android-global mutation with no owner token at
all. The app clears it only from an owned retirement; tethering-service death and a reboot also reset it.
Process death, a force stop and an uninstall release the descriptors and let Binder death take the agent and
the request, but run no clear and leave nothing that could clear it automatically. Residue semantics are in
[`routing.md`](routing.md#rootless-shizuku-mode).

**The one residue this mode must never remove** is the netd counter-chain state naming each dead `testtunN`,
observed accumulating on the qualified Android 17 device and cleared there by a reboot. Deleting it from the
app side is the delete-by-shared-family that [`routing.md`](routing.md#guardrails) forbids;
[`routing.md`](routing.md#rootless-shizuku-mode) records exactly what was observed and leaves other
Android versions unproven.

**Stop and reapply is not universal recovery.** It handles the state an ordered stop can actually fence.
Three failures are outside it: an `UNKNOWN` agent publication and an `UNKNOWN` exact-request publication
both need process death, and tethering connector death needs an app restart.

## Security Boundary

The posture is **best effort by design**, and it is documented rather than mitigated. This mode protects
tethered clients' traffic against the network; it does not defend against other apps on the same device,
because at the app UID there is nothing to defend with.

What the platform does enforce, and what a restricted agent therefore buys: an ordinary UID cannot make a
restricted active `TRANSPORT_TEST` request, an unrestricted one never matches this agent, and it cannot
select this `Network` handle through Java or the NDK, bind a process or socket to it, set its `SO_MARK`, or
use DNS or ping sockets through it.

What is not enforced, established by packet capture on the TUN rather than inferred from a return value:
**any UID that can name the interface can put packets on it.** Restricted-network enforcement guards netId
selection through ConnectivityService and netd; both primitives below select the Linux interface directly, so
neither consults it, and the kernel gates neither.

- `SO_BINDTODEVICE`, which covers TCP as well as datagrams. Its `CAP_NET_RAW` check is conditional on
  `sk->sk_bound_dev_if` being already non-zero (`net/core/sock.c:636`), so the first bind on a fresh socket
  consults no capability at all; only re-binding is privileged. TCP honours it, because the route is
  resolved once at connect time from `sk_bound_dev_if` (`net/ipv4/tcp_ipv4.c:254`).
- `sendmsg` ancillary data, datagram-only in both families: `IP_PKTINFO` assigns the output interface with no
  capability check (`net/ipv4/ip_sockglue.c:295`) and `IPV6_PKTINFO` does the same
  (`net/ipv6/datagram.c:791`). TCP never parses IP-level ancillary data - `sock_cmsg_send` drops it silently
  (`net/core/sock.c:2990`) - which is why the pair matters rather than either alone.

Two things the kernel still gates, which is why the mode is not completely open: `SO_MARK` is
capability-gated (`net/core/sock.c:2943`), matching the observed `EPERM`; and the socket UID stays in the
flow key (`net/ipv4/udp.c:1296`), so Android's UID-range routing rules still apply on top of a chosen
interface.

The consequence is a **one-way injection channel** while a session runs. A local app can push TCP and UDP out
through the selected upstream using this daemon as an unprivileged relay, and can impersonate a tethered
client. It cannot read tunnel traffic, because the TUN descriptor belongs to the daemon, and it has no return
path, because replies are routed downstream and it cannot bind the TUN's addresses. The concrete user-visible
harm is that an app a VPN excludes by per-app rules can reach the internet through it anyway.

Nothing at this privilege level closes it. Hiding the interface name is obscurity, since `testtunN` names and
small interface indices are enumerable; source, tuple, hop-limit and payload filtering are all forgeable;
`setAllowedUids` governs selection, which is the boundary that already holds rather than the one being
bypassed; and owning the TUN descriptor restricts who reads, not who writes. A netd firewall mutation needs
`NETWORK_STACK`, which shell-backed Shizuku does not hold, and a separate namespace, `tc`/eBPF or an OUTPUT
owner match needs root or system authority - either of which would make this root mode again.

So the dataplane is designed to be *sound* under injection rather than to prevent it - parsing, reassembly
bounds and hop-limit validation are what keep forged input from corrupting daemon state - which is also what
retires per-client isolation as a goal. The root [`README.md`](../../README.md) tells users plainly that
this mode protects their tethered clients and not their own device.

## Failure Semantics

[`errors.md`](errors.md) owns the general shape; this section says which failures are terminal for a session.

**Startup failures are terminal, because there is nothing to preserve yet.** Missing authorization, Binder,
permissions or hidden-API access; an unresolvable effective-UID package or privileged-manager field; native
launch, authentication or descriptor-transfer failure; TUN, request, agent, preference, publication or
security-readback failure; unexpected agent or request loss before commit; an immutable MTU or config
mismatch; a foreign TestNetwork collision; and startup or callback deadline expiry. Each rolls back through
the ordered withdrawal above.

**Selected-network recursion is not one of them.** What prevents it in the ordinary path is the published
capability set: with no `NET_CAPABILITY_INTERNET`, the TestNetwork cannot satisfy the app-default request
the egress observation follows. The interface-name check is the fail-closed guard for the case where it is
observed anyway - the TUN is filtered to no egress with a warning, which is a supported steady state rather
than a failure.

**A committed session ends when its own machinery is gone or can no longer be relied on.** A control
connection that cannot carry or confirm an update - the app then cannot tell what the child is bound to and
cannot ask it to stop, and a failed mid-frame write leaves nothing to resynchronize on. An unusable TUN or a
writer whose invariants persistently fail. Agent or request loss after commit, which has already removed the
network the session exists to run. And the tethering connector's death, which additionally requires an app
restart rather than a reapply.

An admission invariant violation is *not* in that list. A stale or double release creates no capacity, is
counted, and appears in the owner reports; there is no path by which it ends a session. Treating it as fatal
would hand forged input a way to stop one.

**Deadlines are operational failures, whatever their shape.** Internal `withTimeout` gates report themselves
as cancellations, so the command path distinguishes them from genuine cancellation - its own job going away,
or a start an explicit stop superseded - and surfaces them through the ordinary user-visible failure path
rather than swallowing them.

**Local and optional failures do not end a session.** Losing Echo or remote-ICMP translation for one family
while TCP and UDP continue; dropping malformed or unsupported packets; a resolver `EBUSY` answered as
SERVFAIL; a nonfatal report the daemon raises about its own state. Because hostile packet input must not
create a report flood, ingress counters are aggregated and reported on epoch change and at exit.

**Rollback never stops Android tethering**, and it cannot remove the `TestNetworkService` singleton the first
acquisition creates inside the system server.

## Qualification Status

Device behaviour is qualified manually. This repository has no instrumented tests and adds none, so anything
not reached below is explicitly unproven, and provenance differs per row rather than being uniform: the
*Topology* column says what each result actually came from. The rooted, stock non-rooted, USB-tethered and
direct-TUN qualification the rows below describe used Android 17 debug builds. The security row is separate
historical evidence from its own harness, and its build provenance is not established by that record.

One consequence of a debug build is worth naming where captures are concerned: the app-UID bootstrap itself
emits debug-only DNS, ping and TCP probes on the selected network before the session's dataplane starts.
They are absent from a release build, so a capture from those passes contains traffic a shipped build would
not produce.

### Verified

| Area | What was established | Topology |
| --- | --- | --- |
| Publication and selection | Restricted `TRANSPORT_TEST` publication with its exact specifier and neither `INTERNET` nor `NOT_RESTRICTED`; Android selecting the session network as the tethering upstream; the delegated `/64` and MTU 1500 reaching the downstream | rooted with root-backed Shizuku, and stock non-rooted with shell-backed Shizuku; USB tethering |
| Attribution | Both branches of the effective-identity path: root-backed Shizuku, and shell-backed Shizuku on a stock non-rooted device | one device each |
| Fail-closed `ARMED` | Packets injected into the TUN while `ARMED` were dropped and created no dataplane state | rooted with root-backed Shizuku; direct TUN input, no external client |
| Dual-stack client traffic | IPv4 and IPv6 ICMP, UDP DNS in both families, and IPv4 and IPv6 HTTPS from a real external client | rooted with root-backed Shizuku, and stock non-rooted with shell-backed Shizuku; USB tethering |
| Selected-VPN egress | With a full-tunnel VPN applied to the app UID and root routing absent, that client traffic passed while both the TestNetwork and the VPN interface counters advanced | rooted with root-backed Shizuku, USB tethering |
| VPN handover | Existing TCP flows reset in both transition directions as designed and fresh connections immediately succeeded; three VPN off/on cycles under UDP and DNS load left no stale residue and no resolver `EBUSY` | rooted with root-backed Shizuku, USB tethering. Not tethering start/stop cycles |
| Root coexistence | With both modes up, root's own policy rules sat ahead of Android's tethering upstream rule and won automatically; starting or stopping either mode changed no identity, state or resource of the other | rooted with root-backed Shizuku, USB tethering |
| Bounded dataplane | IPv4 path-MTU behaviour, 8 KiB IPv4 and IPv6 fragmentation, TCP half-close, NXDOMAIN, malformed-DNS recovery, and 16 concurrent 512 KiB downloads with no fatal, admission or invariant failure | rooted with root-backed Shizuku, USB tethering, one external client |
| Failure injection, partial | Force-stop and app-UID daemon death. Because a config round trip races the authoritative end of the control conversation rather than waiting out its acknowledgement deadline, a child that is simply gone produced prompt network removal and session retirement rather than a deadline-length stall | rooted with root-backed Shizuku, USB tethering |
| Cleanup | Ordered cleanup on normal stop, leaving no network, request, TUN, child or foreground service | rooted with root-backed Shizuku, and stock non-rooted with shell-backed Shizuku |
| Security boundary | Restricted `Network`-handle selection is enforced against a separately signed app and against the owner UID alike; direct interface injection succeeds. Isolation was tested and **failed**, which is why the dataplane treats all TUN input as an unknown local principal | separately signed out-of-repository attacker harness plus TUN packet capture, distinct from the dataplane rows above and proving nothing about release behaviour |

### Open Gates

Do not claim production support until these are resolved.

- **Release builds.** No release-APK qualification exists. Release-build app-UID native launch and
  SELinux/`SCM_RIGHTS` behaviour is untested.
- **Platform coverage.** Android 13-16, OEM builds and Mainline variants: in-process Shizuku provider
  delivery, wrapped descriptor return, exact `ConnectivityManager` field access and constructor-less
  construction, request/agent/tethering selection and cleanup, and packet semantics across all of them.
- **The Wi-Fi hotspot dataplane.** Android's SoftAP failed on the qualification device before this feature
  could be selected, so the whole Wi-Fi path is unqualified.
- **Repeated tethering start/stop cycles**, on USB or any other transport. The VPN off/on cycles above are a
  different exercise and close none of this: nothing has repeatedly cycled the downstream that selection
  depends on.
- **The rest of the failure matrix.** Shizuku death, a Shizuku identity UID change, a child that never
  authenticates, startup races, and the TERM-to-KILL escalation against a child that ignores SIGTERM.
- **Resource ceilings.** Long-horizon descriptor reclamation and the measured descriptor, memory, DNS-token
  and fragment ceilings under sustained pressure on a device. The derivations and their enforcement are
  covered by host tests; watching what a real device produces under real traffic is not.
- **Residual DNS and old-network cases.** The repeated-handover pass did not cover every DNS-over-TCP or
  old-network residue case, nor the daemon's DNS ceiling measured against the platform's per-UID limiter
  across repeated handovers.
- **A truly foreign TestNetwork.** A second agent published by *this* app proves the read path and says
  nothing about ownership; only a separately signed second controller produces a foreign one.
- **Reliability.** None of the above is a reliability, throughput or long-soak claim.

## Related Code And Documentation

Implementation:

- [`mobile/src/main/java/be/mygod/vpnhotspot/shizuku/`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/) -
  session ownership, the command lane, the Shizuku epoch, the privileged manager, the agent, the pinned
  tethering connector and the child conversation, with
  [`ShizukuTetheringService.kt`](../../mobile/src/main/java/be/mygod/vpnhotspot/ShizukuTetheringService.kt)
  supplying process lifetime and the one ordered command path;
- [`mobile/src/main/rust/vpnhotspotd/src/`](../../mobile/src/main/rust/vpnhotspotd/src/) - the app-UID
  dataplane; `bootstrap.rs` and `app_session.rs` are its entry points, `budget.rs` and
  `shared/admission.rs` the resource owners.

Canonical homes for anything this document only summarizes:

- [`routing.md`](routing.md#rootless-shizuku-mode) - the exact external-state and cleanup catalog;
- [`lifecycle.md`](lifecycle.md#app-uid-bootstrap) - bootstrap handshake mechanics, and
  [`#selected-network-handover`](lifecycle.md#selected-network-handover) - the handover sweep;
- [`dns.md`](dns.md) - resolver handoff; [`errors.md`](errors.md) - report shape and terminal versus
  nonfatal; [`traffic.md`](traffic.md) - why MAC-facing accounting does not apply here;
  [`invariants.md`](invariants.md) - the cross-module rules this mode is bound by;
- [`daemon.proto`](../../mobile/src/main/proto/daemon.proto) - the wire schema, including
  `ShizukuSessionConfig`;
- [`mobile/src/hiddenApiStubs/README.md`](../../mobile/src/hiddenApiStubs/README.md) - the system and test
  API inventory, and root [`README.md`](../../README.md) - user-facing limitations and private-API
  compatibility assumptions.
