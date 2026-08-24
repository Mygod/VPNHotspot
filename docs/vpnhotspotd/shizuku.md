# Shizuku Mode Design Handoff

> Status: implementation record for [issue #789](https://github.com/Mygod/VPNHotspot/issues/789).
> The Android side has landed - the mode's own row, its foreground service, the session lifecycle and its
> resource ledger, its own command lane, and the collision classifier - and Step 2's
> device qualification **failed**, so the isolation premise this document was originally written around is
> gone. What landed is narrower than what this document once specified: the mode observes Android's
> tethering rather than driving it, and it starts and stops from one row of its own. Nothing that already
> existed was rewritten to accommodate it. Outside the mode's own files the Android edits are one row added
> to the tethering screen and `DaemonController.daemonCommand` made visible, with its one-time ABI check
> moved into it, so the app UID can exec the same binary. Root mode and this one are wholly independent:
> `RoutingManager` is `master`'s, no root path consults this mode, and this mode consults no root path. `ServiceNotification`
> is `master`'s: this mode registers with it as an interfaceless service already does, which keeps its
> notification alive without adding a word to it. Every service, tile, settings row, `TetheringManagerCompat`
> call and its root fallback behaves exactly as on `master`, and so does every root path bar two explicitly
> authorized ones: the control writer, which is now one cancellation-aware implementation shared with the
> app-UID session because a root write failure has to end the root conversation, and root's IPsec probes,
> whose task ownership and probe-generation model are described under
> [Implementation Status](#implementation-status). See [Security Posture](#security-posture) for what
> replaced it and [Implementation Status](#implementation-status) for what exists.
>
> One thing is claimed carefully. **One rooted device has two narrow passes one revision apart plus an
> exact-current-tree lifecycle smoke, and a separate non-rooted device has an exact-current-tree shell-Shizuku
> dataplane pass.** They are recorded under
> [Device Qualification](#device-qualification-two-passes-one-revision-apart). The first ran *before* the
> root/Shizuku independence correction and covered publication, the fail-closed `Ready` state, `Active`
> selection over USB tethering, one external client's ordinary IPv4/IPv6 ICMP, DNS-proxy and HTTPS/TCP
> traffic, and normal cleanup; what it observed is still evidence only where the correction did not touch
> it, and it is not a pass against the tree as it now stands. The second ran on the corrected
> implementation and covered Shizuku-only USB traffic, automatic root policy-rule precedence without a
> Shizuku transition, return to the same Shizuku session when root stopped, and root survival when Shizuku
> stopped. After the failure-reporting correction, the exact current tree was also built, installed, started
> and stopped once; that final smoke covers publication and ordered cleanup, not the traffic or coexistence
> matrix. Those three used one rooted Android 17 device with a debug build. A separate non-rooted
> device on the same Android version then covered shell-UID Shizuku publication, Android's USB selection,
> external-client IPv4/IPv6 ICMP, DNS and HTTPS, and ordered cleanup. Everything wider than
> that - broader packet semantics, Shizuku VPN egress and handover, isolation, pressure, failure
> injection, a release build, other Android versions, and the Wi-Fi hotspot dataplane - is untested.
> Several step verdicts below - 2,
> 3, 4, 6, and the run recorded under Step 8 - are real and were
> run on hardware, and
> they are kept because what they settled about the *platform* is still true: that the isolation premise
> fails, that a restricted TestNetwork is selected and delegated, that an app-UID binary launches and stops
> cleanly, that the wrapper and the manager construction work. What they are not is qualification of the
> slices as they stand: each was run against the implementation of its own day, and the dataplane, the
> resource model and the session lifecycle have all been rewritten since. So every gate in
> [Production Gates](#production-gates) that the pass above did not reach is still open, and a passed step
> above is
> evidence about a platform question rather than a shipped-behaviour claim. Prose that no status section
> claims is still design and does not describe shipped behavior. The Rust dataplane's own outstanding list
> is now empty - the outer TCP idle floors were the last of it - but everything on it, the IPv4
> Identification reuse window and the floors alike, is covered by the daemon's own tests through its real
> owners and by nothing else. Those tests drive an injected clock, which is not a statement about hardware;
> the MTU-1500 gate that names Identification guards on a device is still open, and so is the rest of the
> matrix.

This is the concise handoff for running VPNHotspot through Shizuku on Android
13 and later. It records the required architecture, security posture, packet
semantics, lifecycle, and production gates. Implementation details should follow
these invariants instead of expanding this document with every test permutation.

## Summary

Shizuku mode shares this app's own default connection with tethered clients
without root - a VPN when Android applies one to this app, and its ordinary
default when none does. It complements root mode rather than replacing it: root
mode stays the full-featured path, and this one trades capability for reach. Read
the limitations below as deliberate scope, not as defects to fix later.

Shizuku's shell or root identity is used only for Android control operations. The
dataplane has no privilege in either case - shell is barely better than the app
UID for this purpose, since neither can touch netfilter - so the daemon runs
under the app UID and the whole design is shaped by having no privileged
dataplane available at all.

Three decisions carry the design; everything else follows from them.

1. **Publish the TUN as a restricted TestNetwork.** `TestNetworkManager` creates
   only the interface. An app-hosted `NetworkAgent` publishes it with
   `TRANSPORT_TEST`, an empty allowed-UID set, and neither `NOT_RESTRICTED` nor
   `INTERNET`. `setupTestNetwork` is never called, because AOSP's setup path adds
   `NOT_RESTRICTED` and would let any installed app select the network. This denies
   `Network`-handle selection but not interface selection, so treat it as raising
   the bar rather than as a boundary; see [Security Posture](#security-posture).
2. **Never touch tethering.** The mode does not start or stop tethering. It sets
   the global `preferTestNetworks` flag and lets Android's own upstream selection
   choose the TestNetwork.
3. **Contain the privileged manager.** The wrapped `ConnectivityManager` is built
   without running a constructor, so the process-wide singleton is never written,
   and it is reachable only through a private context that no app code holds.

What this buys is reach: a rootless mode at all. What it costs is fixed, and the
first item is the one Step 2 added. Any app on the device can push packets into
the TUN by selecting its interface directly, so this mode does not isolate the
tunnel from other local apps. Android's inner IPv4 NAT means clients have no
individual IPv4 identity; only TCP, UDP, DNS, and ICMP are carried; and an
abnormal app death can strand the global preference until the user clears it. Root
mode has none of these limits, which is why they are acceptable here and why root
mode stays the recommended path.

Dual-stack is in scope. Clients get IPv6 from a documentation-prefix `/64` that
Android will delegate, paired with a ULA resolver address, as described under
[IPv6 Availability](#ipv6-availability) - though Android delegates it to only one
downstream, so a second tethered interface is IPv4-only.

Evidence is uneven, and the gaps are the schedule risk:

- **Source-verified across Android 13-17.** Tethering will select a restricted
  TestNetwork with no `INTERNET` capability; the preference belongs to
  `ITetheringConnector` and needs `NETWORK_SETTINGS`; `requestNetwork` is checked
  against the caller's AppOps package, so a shell-backed session must present
  shell's; resolver cancellation stops nothing, and the resolver's per-UID limit
  is 256 with `-EBUSY` on exhaustion. Every hidden descriptor is checked against
  `../hiddenapi/hiddenapi-flags.csv`.
- **Device-qualified, including one failure.** The real Shizuku wrapper, the
  constructor-less manager, effective-UID attribution, the restricted agent, the
  exact request, and complete cleanup all work on an Android 17 device. A
  separately signed attacker and the owner UID both reached the TUN through direct
  interface selection, which is the failure [Security Posture](#security-posture)
  is written around.
- **Device-qualified, in favour of the design.** Tethering selects the restricted
  TestNetwork, delegates the documentation-prefix `/64` to the downstream, installs
  IPv4 forwarding and MASQUERADE toward the `/30`, and propagates MTU 1500. See
  [Step 3 Verdict: Passed](#step-3-verdict-passed).
- **Unproven.** Android 13-16, OEM and Mainline variants, and every measured budget in this document. Client-side
  connectivity through a dataplane is no longer wholly unproven - one USB client's
  ordinary IPv4/IPv6 ICMP, DNS-proxy and HTTPS/TCP traffic passed one revision before the independence
  correction, on a path that correction did not touch, see
  [Device Qualification](#device-qualification-two-passes-one-revision-apart). The same device then proved
  the monitored-interface coexistence rule in both directions on the corrected implementation. Everything
  wider is still unproven: Shizuku VPN egress and handover, fragmentation and path-MTU, TCP edge cases
  and DNS failure modes, and any client on another Android version or transport.

Two shipped prototypes,
[v2rayNG#5903](https://github.com/2dust/v2rayNG/pull/5903) and
[shizzi](https://github.com/carlelieser/shizzi), already drive Android tethering
onto a Shizuku-created test network, which retires the largest feasibility
question: the mechanism works on real devices. Both publish through
`setupTestNetwork`, so what they demonstrate is the *unrestricted* variant. They
are therefore prior art for the plumbing and the negative control for the hardening
in [Restricted TestNetwork Publication](#restricted-testnetwork-publication) at the
same time, and shizzi documents the consequence directly, warning that "IPv6 is not
suppressed on the downstream; v6 traffic may bypass the tunnel". Step 2 showed the
gap between them and this design is narrower than intended: they are open to
`Network` selection as well, where this design is open only to interface selection.

The risks that would change the direction rather than the details:

- **Settled, against the design: the restricted boundary does not hold.** Both a
  separately signed app and the owner app's own UID reached the TUN on Android 17.
  This was the security premise; the mode continues without it, under the
  best-effort posture below, rather than being abandoned.
- **Settled, in favour of the design: prefix delegation follows `LinkProperties`,
  not capabilities.** A restricted agent's documentation-prefix `/64` is delegated
  to the downstream on Android 17, so dual-stack does not depend on the
  unrestricted network the prototypes use.
- **Settled: tethering rejects neither a `/30` IPv4 upstream nor the restricted
  agent.** Both were exercised on device; widening the prefix stays the recovery if
  another release disagrees.
- **Settled, and it decides the UI: `RESTART_REQUIRED` is the norm, not an edge
  case.** Tethering that already holds an ordinary upstream does not move when the
  preference is set, because the preference is not a reselection trigger. A
  tethering restart selects the TestNetwork immediately. An OEM that disables
  automatic upstream selection cannot run this mode at all, which is detectable on
  Android 13 and forced on from Android 14.
- **Shizuku cannot return the TUN `ParcelFileDescriptor`** through the wrapper on
  some Mainline variant, which removes the only path to the interface.

Trust this document unevenly, because it earned confidence unevenly.

- **Verified.** Every platform claim carries an exact `android-*_r1` tag and line:
  request attribution, the tethering connector and `NETWORK_SETTINGS`, upstream
  eligibility, the IPv6 delegation gate, resolver cancellation and the per-UID
  limiter, the jarjar relocation, and `destroyForcibly()` sending SIGTERM. Take
  these as settled; re-deriving them is expensive and they are why this document
  exists.
- **Was intent; now implemented.** The handover, the upstream-generation and
  downstream-epoch model, and the epoch protocol inside
  [Startup And Commit](#startup-and-commit) were revised in ten consecutive review
  rounds before any of them existed, and this entry used to warn that the mechanics
  were prose no compiler had checked. That has since happened: they are implemented
  against the real types, the real proto and real task ownership, they are covered by
  the host record in [Implementation Status](#implementation-status), and the wording
  that did not survive contact was rewritten rather than left standing. The invariants
  are unchanged. What is still unproven is the *device* rather than the mechanics -
  see [Production Gates](#production-gates).
- **Audited once, before Step 5.** [Dataplane Semantics](#dataplane-semantics) and
  [Resource Policy](#resource-policy) came from the first draft and had received none
  of the scrutiny the control plane did. That pass has now happened and it changed
  them: per-principal budgets are gone, the identity ceiling is measured and binds
  before the descriptor ceiling, the identity quarantine is removed outright and
  replaced by the interface check alone, the writer's two
  backpressure sources are separated, and the IPv4 DF floor is decided explicitly rather than
  left implied - by the session's own fixed MTU, since this mode reads nothing about the
  downstreams behind it. Everything below that the dataplane slices have not reached yet is
  still first-draft prose.

Implementation order is in [Implementation And Validation](#implementation-and-validation)
and is sequenced by risk: the boundary and the platform path are proven on device
before any dataplane exists. That sequencing did its job - the boundary failed in
days, on a device, with no dataplane written - even though the outcome was to
redefine the mode rather than to end it.

## Security Posture

The design philosophy is **best effort**. This mode protects tethered clients'
traffic; it does not defend against other apps on the same device, because at the
app UID there is nothing to defend with.

What the platform does enforce, and what a restricted agent therefore buys:

- an ordinary UID cannot make a restricted active TEST request, and an
  unrestricted TEST request never matches this agent;
- it cannot select this `Network` handle through Java or the NDK, bind a process
  or socket to it, or set its `SO_MARK`;
- it cannot use DNS or ping sockets through the handle.

What nothing enforces, verified by packet capture on the TUN rather than inferred
from a return value: any UID that can name the interface can put packets on it.
Restricted-network enforcement guards netId selection through ConnectivityService
and netd, and both of these select the Linux interface directly, so they never
consult it. Two independent primitives, and the kernel gates neither:

- `SO_BINDTODEVICE`, which covers **TCP as well as datagrams**. Its `CAP_NET_RAW`
  check is conditional on `sk->sk_bound_dev_if` being already non-zero
  (`net/core/sock.c:636`), so the first bind on a fresh socket consults no
  capability at all; only re-binding or unbinding is privileged. TCP honours it,
  because the route is resolved once at connect time from `sk_bound_dev_if`
  (`net/ipv4/tcp_ipv4.c:254`).
- `sendmsg` ancillary data, datagram-only, both families: `IP_PKTINFO` assigns
  `ipc->oif = info->ipi_ifindex` with no capability check
  (`net/ipv4/ip_sockglue.c:295`), and `IPV6_PKTINFO` does the same
  (`net/ipv6/datagram.c:791`). TCP never parses IP-level ancillary data - it is
  silently dropped by `sock_cmsg_send` (`net/core/sock.c:2990`) - so this primitive
  is UDP/ping-only, which is why the pair matters rather than either alone.

Two things the kernel does still gate, which is why the mode is not completely
open: `SO_MARK` is capability-gated (`net/core/sock.c:2943`), matching the observed
`EPERM`; and `sk->sk_uid` stays in the flow key (`net/ipv4/udp.c:1296`), so
Android's UID-range routing rules still apply on top of a chosen interface.

The consequence is a one-way injection channel while a session runs. An app can
push TCP and UDP out through the selected upstream using this daemon as an
unprivileged relay, and can impersonate a tethered client. It cannot read tunnel
traffic, because the TUN descriptor belongs to the daemon, and it has no return
path, because replies are routed downstream and it cannot bind the TUN's
addresses. The concrete user-visible harm is that an app excluded from a VPN by
per-app rules can reach the internet through it anyway.

Nothing available at this privilege level closes it. Hiding or randomizing the
interface name and addresses is obscurity; `testtunN` names and small interface
indices are enumerable. Source, tuple, hop-limit, and payload filtering are all
forgeable. `setAllowedUids` governs selection, which already worked and was
bypassed. Owning the TUN descriptor restricts who reads, not who writes. Android's
UID/interface BPF rule is for VPN ingress, not this egress case. A netd firewall
mutation needs `NETWORK_STACK`/`MAINLINE_NETWORK_STACK`, which shell-backed
Shizuku does not hold; a separate namespace, `tc`/eBPF, or an OUTPUT owner match
needs root or system authority. Either of those would make this root mode again.

So this is documented rather than mitigated, and the dataplane is designed to be
*sound* under injection rather than to prevent it: every packet from the TUN is
treated as untrusted input from an unknown local principal. Concretely, that
retires per-client isolation as a goal - see
[Classification And Principals](#classification-and-principals) - and it makes
resource budgets self-protection rather than fairness. Correctness properties
(parsing, reassembly bounds, hop-limit validation) are
unaffected and still required, because they are what keeps forged input from
corrupting daemon state.

## Goals And Non-Goals

Shizuku mode is a rootless alternative to the existing UID-0 daemon mode:

- Android control operations use Shizuku's effective shell/root identity.
- The packet daemon and all selected-network sockets run under the app UID.
- Android tethering remains externally managed. This mode never calls
  `startTethering` or `stopTethering`.
- Root and Shizuku controllers, processes, and commands remain separate and
  independent: neither starts, stops, delays, refuses or rebuilds the other, and
  both may run at once. Root's own routing then takes precedence over whatever
  upstream Android selected, by the existing root design.
- The mode supports TCP, UDP, virtual DNS, dual-stack Echo, selected ICMP error
  translation, ingress reassembly, and bounded return fragmentation.
- It does not provide arbitrary raw-IP forwarding for ESP, GRE, SCTP, unknown
  protocols, or downstream link-control traffic.
- Only one TestNetwork controller is supported while the mode is in use.
- It does not isolate the tunnel from other apps on the device, and does not
  attempt to. Any local app can inject packets into the TUN; see
  [Security Posture](#security-posture). Root mode does not share this limitation.
- It therefore provides no per-client identity or per-client isolation at all, in
  either address family. Every packet from the TUN is untrusted input from an
  unknown local principal.
- Losing the TestNetwork does not stop tethering. Android reselects an ordinary
  upstream and clients keep working unprotected. This mode cannot prevent that;
  see [Upstream Fallback](#upstream-fallback).
- App death can leave Android's global `preferTestNetworks` flag set. This is an accepted
  limitation. The next session adopts the flag rather than clearing it at startup, so what
  clears it is that session's own stop, or a reboot; there is no separate recovery surface
  and no automatic cleanup guarantee.

## Architecture And Ownership

| Component | Responsibility | Lifetime |
| --- | --- | --- |
| App process | Single ordered controller, private privileged manager and tethering connector, TUN PFD, exact request, restricted `NetworkAgent`, tethering observation, selected-network choice, Rust child | Entire Shizuku session |
| App-UID `vpnhotspotd` | TUN packet engine and all outbound sockets | Authenticated control connection |
| ConnectivityService/netd | Native TestNetwork, routes, and restricted socket permission | Agent Binder |
| Android tethering/netd | Downstream forwarding and inner IPv4 NAT/conntrack | External tethering lifecycle |
| Selected VPN/default `Network` | Egress for Rust-created sockets | Current handle in the daemon's session config |

The app process is the only controller and resource owner. All callbacks, user
commands, child exits, Binder changes, and replacements feed one ordered state
machine. No helper process, Shizuku UserService, generic Binder relay, or second
copy of ordinary application initialization is required.

### Privileged Manager Containment

The session needs one `ConnectivityManager` backed by the Shizuku-wrapped
`IConnectivityManager` for exactly three direct operations: the exact foreground
request, its release, and the agent's `CONNECTIVITY_SERVICE` lookup.

Those three are not the only framework access points it is reached through, which
is what makes "restricted to three" a containment claim rather than a call-site
count. Two more sit inside the custom agent's own publication: the `NetworkAgent`
constructor probes `isFeatureEnabled` on releases that have it - answered from the
cache warmed in step 2 below, so it issues no wrapped transaction
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-16.0.0_r1/framework/src/android/net/NetworkAgent.java#624)) -
and `NetworkAgent.register` reaches `ConnectivityManager.registerNetworkAgent`.
Both authorize on UID alone, and neither touches the aliased collections below.

Every hidden `ConnectivityManager` constructor can assign the private static
`ConnectivityManager.sInstance`. Android 13 assigns it unconditionally; Android
14-17 assign it only when the singleton is still null, and then publish a
*second* manager built on the application context rather than the constructed one
([Android 13 source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/ConnectivityManager.java#2626),
[Android 17 source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#2944)).
Do not invoke any of them. Build the private manager without running a
constructor so the process-wide static is never written on any release:

1. Initialize
   [`UnblockCentral`](../../mobile/src/main/java/be/mygod/vpnhotspot/util/UnblockCentral.kt),
   resolve `ConnectivityManager`'s declared instance fields, and prove that
   `mContext` and `mService` can be read and assigned before creating the
   privileged manager.
2. Resolve the application context's ordinary `ConnectivityManager`. It is the
   only correctly constructed instance in the process, so it is the
   initialization template. Where the release declares the lazy feature cache,
   warm it on that ordinary manager first through its own `isFeatureEnabled`, so
   the copy inherits a populated value over the ordinary binder instead of
   leaving a lazy query for the first privileged call.
3. Allocate an uninitialized `ConnectivityManager`. `Unsafe.getUnsafe()` rejects
   app-classloader callers
   ([source](https://android.googlesource.com/platform/libcore/+/refs/tags/android-17.0.0_r1/ojluni/src/main/java/sun/misc/Unsafe.java#63)),
   so reach the singleton through the `theUnsafe` field and call
   `allocateInstance`; both members are `unsupported` rather than `blocked` on
   Android 13 and 17 alike. There is **no** JNI `AllocObject` fallback: earlier
   drafts named one, but this app ships no native code in its own process and
   adding a library for one call is not worth it against a member greylisted
   unchanged across the whole supported range, so absence is terminal for the
   session instead - which costs only the mode, since it happens before any TUN,
   request, preference or agent mutation. Copy every declared non-static field
   from the ordinary manager, then assign the private `ContextWrapper` to
   `mContext` and the pinned Shizuku-wrapped binder to `mService`.
4. Require identity-equal readback of both assigned fields and an unchanged
   `sInstance` before publishing the manager to the session owner.

Copying every declared field instead of a per-release minimum is deliberate:
field initializers run in the skipped constructor and the field set is
Mainline-dependent. Android 13-15 dereference only `mContext`/`mService` on these
paths, but Android 16-17 `requestNetwork` also reads
`mEnabledConnectivityManagerFeaturesLock`
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#4751)),
and an updated Connectivity module can add more on any release. Inheriting live
values from the ordinary manager stays correct without an `SDK_INT` branch.

Every field except `mContext` and `mService` is aliased, not owned. On Android
13-17 that is `mTetheringManager`, `mNetworkActivityListeners`,
`mTetheringEventCallbacks`, and `mQosCallbackConnections`, plus
`mEnabledConnectivityManagerFeaturesLock` and its cached value on 15 and later.
Both managers therefore mutate one set of collections and share one monitor, so
the private manager is restricted to the three direct operations and the two agent
access points above, none of which touches them. Default-network
activity listeners, the `ConnectivityManager` tethering shims and their event
callbacks, QoS callbacks, and every other API backed by that shared state are
forbidden on it. Re-enumerate the aliased set and its exact descriptors at
adoption; it is release-dependent.

`sInstance` is read as an invariant, never written, and the manager cached by
`Context` is a separate reference this design never touches. No ordinary
framework consumer is ever exposed to the privileged manager, so there is no
replacement window to audit, no restoration to prove, and no poisoned-process
state: any readback failure is terminal for the session only, before TUN,
request, preference, or agent mutation.

Give the private manager and the custom `NetworkAgent` one shared private
`ContextWrapper` that:

- returns the privileged manager from the string-based `getSystemService` only
  for `CONNECTIVITY_SERVICE`, and delegates `getSystemServiceName` normally so
  the final typed `getSystemService(ConnectivityManager::class.java)` resolves
  through that override;
- returns an operation package matching the pinned effective UID and a null
  attribution tag;
- delegates every other service and lookup to the application context.

The attribution override is mandatory, not cosmetic. `ConnectivityManager`
forwards `mContext.getOpPackageName()` with every request
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#4758)),
and ConnectivityService verifies it against the Binder calling UID through
`AppOpsManager.checkPackage`
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#9330)).
Shizuku makes that UID shell or root, so the app package throws for a
shell-backed session: `AppOpsService` rejects a package that does not belong to
uid 2000
([source](https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-17.0.0_r1/services/core/java/com/android/server/appop/AppOpsService.java#5218)),
while uid 0 skips the package check entirely
([source](https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-17.0.0_r1/services/core/java/com/android/server/appop/AppOpsService.java#5119)).
Resolve the package from the pinned effective UID, which is `com.android.shell`
for shell; root owns no package and accepts any installed one, so the app
package remains correct there. An unresolvable package is terminal before
mutation. This is the only package-attributed call in the design: the restricted
request branch, agent registration, TUN creation, and the tethering preference
all authorize on UID alone.

Never return that context or manager from an app service, singleton, or generic
IPC surface.

`TestNetworkManager` is reached over the wrapped binder, not through the private
manager. The interface binder is handed out only by
`IConnectivityManager.startOrGetTestNetworkService()`, which enforces
`MANAGE_TEST_NETWORKS` on the calling UID
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#14418)),
so retain the wrapped `IConnectivityManager` separately and call it directly. That
keeps the private `ConnectivityManager` to the contained surface above, which is
what makes its aliased fields safe; routing this through it instead would widen a
manager whose collections are shared with the ordinary one. The call is a wrapped
transaction and belongs in the pinned epoch like any other.

That call is not a lookup on its first use per boot. `ConnectivityService`
constructs `TestNetworkService`, whose constructor starts a handler thread and
registers a `NetworkProvider`, and caches it with no teardown path
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/TestNetworkService.java#80)).
That residue is framework-owned and lives for the system server's lifetime, so it
outlives rollback, session stop, and app-process death alike, and no cleanup path
here can remove it. It is bounded to one thread and one provider per boot no
matter how many sessions run, and any test-network user creates the same thing,
so it is recorded rather than mitigated - but "rollback removes every completed
step" does not extend to it.

The returned binder is raw. `ConnectivityManager.startOrGetTestNetworkManager()`
would hand back a manager wrapping it unwrapped
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#6130)),
and every later `createTunInterface` would then transact from the app UID and
fail the same permission check. Wrap the returned binder in turn, then build the
manager from it.

The interface type is jarjar-relocated on some releases and not others, so do not
name it. Android 13 uses a static allowlist of rules, which does relocate some
`android.net` types such as `NetworkFactory`, but names no TestNetwork entry, so
the type is `android.net.ITestNetworkManager` there. From Android 14 the module
generates its rules and relocates by default with a named exclude list, giving
`android.net.connectivity.android.net.ITestNetworkManager`. A Mainline update can
move this without moving `SDK_INT`, so a version branch is the wrong shape of
answer.

Derive the name instead of resolving it. `TestNetworkManager` has a single
one-argument constructor, and its parameter type *is* the interface under
whichever name that build uses; load `$Stub` from that type's own name and invoke
`asInterface` on it. The relocation then stops being a compatibility question at
all.

### Shizuku Binder Ownership

Use the ordinary in-process Shizuku provider path. Do not enable Shizuku
multiprocess support or call `requestBinderForNonProviderProcess`. Install sticky
Binder-received and Binder-dead listeners, verify the expected effective UID and
required permissions, and pin one Binder identity/epoch for the session.

`ShizukuBinderWrapper` dispatches through mutable process-local Shizuku state.
Before and after every wrapped transaction, compare the current Binder identity,
epoch, and liveness with the pinned value. For an asynchronous result, repeat the
check when consuming the callback. Any death or change makes the result unknown:
do not commit, close returned descriptors, and run rollback. Bracket the complete
privileged operation as well, because framework code can issue transactions the
owner cannot check individually: a mismatch at either boundary is unknown, and
passing per-transaction checks across two epochs is not a commit. Construction
removes the only known multi-transaction case by warming the feature cache, so
each operation issues exactly one wrapped transaction; treat any newly observed
second transaction as a compatibility finding rather than a tolerated case. A
replacement epoch never resumes or mutates a committed session. After admission is closed, cleanup may build a new
private manager and connector from a freshly authorized epoch with the same
effective UID solely to clear the preference and release the exact retained
request; it must not republish an agent or admit traffic.
ConnectivityService authorizes request release against the stored owner UID, so
Binder identity continuity is unnecessary for this cleanup exception but
effective-UID equality is mandatory
([AOSP](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#6683)).

Shizuku death after commit does not immediately remove the request or agent.
Existing traffic continues only through the already-owned TUN and
selected-network dataplane, with no ordinary-network fallback, but further
wrapped calls from the dead epoch are unavailable. Direct agent withdrawal,
local FD closure, a cleanup-only replacement epoch, and eventual app-process
Binder death are the cleanup paths.

### One Session Generation

The owner atomically accepts one session generation and rejects every overlapping
session start or restart, including while stopping. This is separate from
selected-network replacement, which is a daemon config update rather than a
session transition. Every
callback and child event carries that generation; late events from a retired
generation cannot commit, release, or resurrect state. A new session starts only after the previous local
resources are retired and ConnectivityService no longer exposes its agent.

## Restricted TestNetwork Publication

This section used to be called a security boundary. It is not one: it denies
`Network`-handle selection, which Step 2 confirmed, and does not deny interface
selection, which Step 2 also confirmed. Keep every requirement below anyway - it
removes the easy path, costs nothing, and is what the `setupTestNetwork`
prototypes get wrong - but read it as hardening, and read
[Security Posture](#security-posture) for what is actually guaranteed.

Never call `TestNetworkManager.setupTestNetwork` or `teardownTestNetwork`.
AOSP's setup path adds `NET_CAPABILITY_NOT_RESTRICTED`, which would let ordinary
apps select the TestNetwork.

Use `TestNetworkManager` only for `createTunInterface`. Publish the TUN with an
app-hosted custom `NetworkAgent` whose initial state is complete and immutable:

- exactly one transport: `TRANSPORT_TEST`;
- exact `TestNetworkSpecifier(testtunName)`;
- `allowedUids = emptySet()`;
- no owner or administrator UID metadata;
- no `NOT_RESTRICTED` and no `INTERNET`;
- `NOT_SUSPENDED`, `NOT_VCN_MANAGED`, `NOT_VPN`, and `NOT_METERED`;
- `NOT_BANDWIDTH_CONSTRAINED` only where the installed module exposes and
  preserves it;
- legacy type `TYPE_TEST` and score 1.

This produces a restricted netd network with `PERMISSION_SYSTEM` and no ordinary
UID exception. The owner app itself does not need socket access to the
TestNetwork; it uses only the TUN FD.

Other apps may discover the `Network` handle, and secrecy was never the
mechanism. Ordinary UIDs, including apps with `CHANGE_NETWORK_STATE`, are denied
by the platform and must stay denied:

- making a restricted active TEST request;
- receiving this agent from an unrestricted TEST request;
- binding a process or socket to the handle, including `SO_MARK`;
- using DNS or ping sockets through the handle.

Selecting the interface or a source address directly is **not** denied, on any
release qualified so far. That was listed here as a requirement in earlier drafts;
Step 2 disproved it, and it is now a documented property of the mode rather than
something an implementation can assert.

Shell/root principals, including another separately user-authorized
Shizuku/Dhizuku controller, remain trusted peers and are out of scope.

### Exact Foreground Request

Before publishing the agent, create:

```kotlin
NetworkRequest.Builder()
    .clearCapabilities()
    .addTransportType(NetworkCapabilities.TRANSPORT_TEST)
    .setNetworkSpecifier(testtunName)
    .build()
```

The builder order is load-bearing: the deprecated `String` overload only yields a
`TestNetworkSpecifier` because `TRANSPORT_TEST` is already set, and otherwise
produces an `EthernetNetworkSpecifier` that can never match this agent
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkRequest.java#559)).

Register it only with
`requestNetwork(exactRequest, callback, ownerHandler)` through the private
wrapped manager. Do not use passive `registerNetworkCallback`: observation does
not keep a low-score agent wanted. Retain the callback Binder until after agent
withdrawal and release it through the same effective privileged caller.
Callback-Binder death is the final release path when explicit release is
unavailable or unknown.

### Link Properties

Construct immutable `LinkProperties` before agent registration:

- exact TUN interface name and re-read link addresses;
- one directly connected route for every address/prefix;
- family default routes through the TUN, including an IPv6 default route, which
  tethering requires alongside the global `/64` before it delegates a prefix;
- configured virtual DNS servers, including an IPv6 one so proxied DNS has an
  upstream target;
- MTU 1500.

ConnectivityService adds directly connected routes during registration, so
precompute them and require the normalized readback to match. At the publication
barrier, also require the exact TEST transport/specifier and absence of
`NOT_RESTRICTED` and `INTERNET`. Permit only documented
ConnectivityService-managed capabilities such as `FOREGROUND`.

#### Addressing

Every address this interface occupies is an address clients cannot reach. The
default route delivers their traffic to the TUN and the daemon re-originates it
upstream, but addresses inside the connected prefix are delivered locally
instead, and the virtual DNS addresses are intercepted by exact match. The TUN's
address space is therefore a hole punched in the client-reachable internet, which
sets both rules: keep it small, and put it where nothing real lives.

Take the IPv4 addresses from TEST-NET-1, `192.0.2.0/24` (RFC 5737), matching the
documentation prefix used for IPv6. Both are guaranteed never to be assigned, so
the hole cannot collide with a destination a client legitimately wants, and both
are recognizable on sight in a capture or a bug report as the synthetic upstream
rather than a real network. A randomly chosen subnet is strictly worse: identical
mechanics, but the hole lands in space somebody uses, and the failure presents as
a bug in this app rather than as an addressing choice.

Use the smallest prefix that the platform actually accepts. The interface address
plus the virtual DNS addresses is the whole requirement, and the DNS addresses
need not share the prefix, since the default route delivers them to the TUN
anyway and classification is by exact address match against
`session_virtual_addresses`. Do not push to a host route to save two addresses:
tethering's IPv4 path is not qualified against a `/32` upstream, and Android's
`MASQUERADE` rewrites client sources to this interface's address, so the address
must be present and usable before anything is forwarded.

Name every one of these as a constant with its RFC purpose in a declaration doc
comment, IPv4 host, IPv4 virtual DNS, IPv6 `/64`, and IPv6 resolver alike. They
are compatibility-relevant hardcoded values, not implementation trivia.

Shell cannot read back `allowedUids` reliably on Android 13-17, and the
implementation submits an empty set by not calling the blocked setter at all,
since a fresh builder already carries one. Same-app and cross-app *handle*
selection denial is qualified on Android 17 and remains a gate on other releases.
Interface-level injection is denied on none of them.

## Android Platform Boundaries

### Tethering Selection

The preference belongs to tethering, not connectivity. Drive it through a pinned
Shizuku-wrapped `ITetheringConnector` and consume its `IIntResultListener`
result, reusing the connector acquisition and result-listener plumbing already in
[`TetheringManagerCompat`](../../mobile/src/main/java/be/mygod/vpnhotspot/net/TetheringManagerCompat.kt).
Do not call `TetheringManager.setPreferTestNetworks`: it discards the result code
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#2241))
and blocks the caller for up to its own 60-second timeout. Both entry points are
`blocked`, so the connector is chosen for the result code, not for reach.

Both preference writes are issued from the owner lane, and that is what keeps them
ordered. `ITetheringConnector` is declared `oneway`, so the transact returns
without waiting on the service and there is no stall to own; the result arrives
separately through the listener and carries the usual deadline. `IBinder` does
guarantee ordering for one-way calls made to the *same* Binder object, across
caller threads as well as within one, so the invariant that matters here is not
"same thread" but "same object": both writes go to the one pinned connector
binder of one epoch, and the single controller is what sequences them. Offloading
the clearing `false` to a different connector - a replacement epoch's, say - is
what could let the service apply them in reverse and strand the preference, which
is why cleanup carries a pending clear rather than racing one. No
run-to-completion record is needed for a call that cannot block.

The result code is required because denial is silent: without
`NETWORK_SETTINGS`, `TetheringService` reports
`TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION` through the listener instead of
throwing
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/TetheringService.java#286)).
Include `NETWORK_SETTINGS` in the pinned-identity admission checks. AOSP shell
holds it, but it is an OEM-variable grant.

`setPreferTestNetworks(true)` only changes a global preference; it does not
force upstream reselection. `Tethering` posts the flag and always answers
`TETHER_ERROR_NO_ERROR`
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/Tethering.java#3109)),
and `UpstreamNetworkMonitor` only stores it
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/UpstreamNetworkMonitor.java#699)).

Once reselection does run, a restricted TestNetwork is eligible: the upstream
monitor listens with cleared capabilities and its preference path matches on
`TRANSPORT_TEST` alone, requiring neither `INTERNET` nor `NOT_RESTRICTED`
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/UpstreamNetworkMonitor.java#328)).
Reselection runs on a default-network switch, or on any upstream event while
tethering currently has no upstream interface
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/Tethering.java#2265)).

All of this assumes automatic upstream selection. `chooseUpstreamType()` consults
`getCurrentPreferredUpstream()` only when `chooseUpstreamAutomatically` is set,
and falls back to the legacy `selectPreferredUpstreamType()` over a configured
type list otherwise
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/src/com/android/networkstack/tethering/Tethering.java#1798)),
and only the former reads `mPreferTestNetworks`. On a build where an overlay turns
`config_tether_upstream_automatic` off, the preference is never consulted at all,
so the TestNetwork can never be selected and cycling tethering does not help.

That is only reachable on Android 13. From Android 14 the tethering module forces
automatic mode on regardless of the resource, because `forceAutomaticUpstream` is
`SdkLevel.isAtLeastU()`
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/TetheringConfiguration.java#219)),
so no OEM overlay can disable it. AOSP also declares the resource `true`, which is
why this was an OEM variation rather than the norm even on 13; note that the value
passed for a *missing* resource is `false`, so absence and an explicit `false` are
indistinguishable through this path.

Detect it rather than leaving users in a state that cannot recover: the tethering
package is already resolved for connector acquisition, so read that boolean from
its resources on Android 13 and refuse to start with an explicit unsupported-device
error. A permanent `RESTART_REQUIRED` that no user action can clear is the worst
available outcome. A resource that cannot be read at all is a warning rather than a
refusal, because an unreadable resource is indistinguishable from a wrong package
lookup and refusing would break a working device, while guessing the other way
degrades to a visible `RESTART_REQUIRED`.

Set the preference after Rust is ready but before agent publication. If
tethering starts afterward, or is running without an upstream, the TestNetwork
can be selected normally. If tethering is already using an ordinary upstream, the
committed state is `RESTART_REQUIRED` until Android reevaluates or the user
independently cycles the separate tethering toggle.

Step 3 measured which of those two cases is the common one, and the answer shapes
the UI: a session started while tethering already holds an ordinary upstream stays
`RESTART_REQUIRED` indefinitely. Setting the preference is not itself a reselection
trigger, exactly as the source above says, so `RESTART_REQUIRED` is the *normal*
outcome for "turn the mode on while the hotspot is already running" rather than an
edge case. The mode still must not cycle tethering itself; the UI has to ask the
user to, and should say so plainly rather than presenting it as an error.

#### Selection Is Racy

Cycling tethering is also not a *reliable* fix, which Step 7 found by accident while
exercising the config channel. `UpstreamNetworkMonitor.stop()` releases its listen
callback and calls `mNetworkMap.clear()`
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/UpstreamNetworkMonitor.java#192)),
and `getCurrentPreferredUpstream()` looks for the test network in that map
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/UpstreamNetworkMonitor.java#328)).
So a tethering start re-registers the callback and evaluates its upstream while the
map may still be empty: `findFirstTestNetwork` returns null, tethering settles on the
default network, and because it now *has* an upstream, the later callback that adds
this session's network re-triggers nothing.

The flag itself is never reset - `stop()` does not touch `mPreferTestNetworks` - and
re-issuing the preference does not help, because `setPreferTestNetworks` only assigns
the field and answers the listener
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/Tethering.java#3109)).
Nothing available to an app forces a reselection: this network deliberately has no
`INTERNET` capability, so it can never become the default network whose switch is the
other trigger.

Observed both ways on the same device and build, with the same code: one session
reached `ACTIVE` on the first cycle, another stayed `RESTART_REQUIRED` across three.
So `RESTART_REQUIRED` may need the user to cycle tethering more than once. The row does not
say so: it carries terse status like every other row on the page, and the remedy belongs in
`README.md` rather than under a switch. This is the strongest argument yet for a future
Android change that lets a caller request reselection, and it should be recorded as such
rather than papered over.

The Shizuku switch never starts or stops tethering. Preference success is not
proof of upstream selection; only the tethering callback naming the exact
session `Network` grants `ACTIVE`.

### Android's Inner IPv4 NAT

Android installs tethering forwarding and MASQUERADE before packets reach the
TestNetwork TUN. Rust therefore sees Android-translated IPv4 addresses, ports,
and Echo identifiers, not physical-client IPv4 identities.

The resulting path is explicit double NAT:

1. Android owns inner IPv4 mapping, filtering, reverse conntrack, and timeout.
2. Rust owns its outer selected-network TCP/UDP/Echo state.

Consequences:

- all ordinary IPv4 traffic is attributed to one shared `platform_ipv4`
  principal;
- Rust cannot promise end-to-end endpoint-independent mapping, filtering, or
  lifetime semantics across Android's inner NAT;
- Android alone restores replies and translated errors toward physical clients;
- Rust timers never mirror or refresh Android conntrack.

### Android-Proxied DNS

Android may originate forwarded DNS traffic from a TestNetwork-local address,
especially for IPv6 RDNSS. Classify all accepted virtual-DNS traffic as the
shared `platform_dns` principal. Source address, port, query ID, family, or
network generation never creates a physical-client identity.

Earlier drafts admitted direct IPv6 client traffic only against a unique owner
established by platform metadata or proven downstream anti-spoof enforcement, and
dropped forgeable sources before allocation. That rule cannot be satisfied: any
local app can put an arbitrary IPv6 source on the TUN, so *every* source is
forgeable and the rule would admit nothing. Direct IPv6 traffic is therefore
admitted under one shared principal, exactly like `platform_ipv4`, and carries no
client identity.

### IPv6 Availability

Tethering copies only link addresses that are `isGlobalPreferred()` with a
prefix length of exactly 64 from its upstream into the downstream config
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#239)),
and that predicate rejects ULAs by name
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/LinkAddress.java#487)).
The deterministic `fd...` prefix root mode uses therefore cannot unlock IPv6 for
clients here. Note that the same file admits ULAs as DNS servers through a looser
predicate; that applies to resolvers, never to the delegated prefix.

Every client packet is re-originated from the selected network's own source
address, so whatever prefix clients receive never appears upstream. The prefix
only has to satisfy the platform predicate and avoid colliding with destinations
clients want to reach. Of the three candidates, a ULA fails the predicate and
yields no client IPv6 at all; a real global prefix the app does not own satisfies
the predicate but collides with genuine destinations, which is the worst option;
and the documentation prefix satisfies the predicate and cannot collide, because
that space is guaranteed never to be assigned.

Take the documentation prefix. The first draft rejected it on RFC 3849 grounds,
but within a fully translated dataplane those addresses are local and unroutable
by design, and this document already accepts the same reasoning on the IPv4 side
by using TEST-NET space for the TUN. Rejecting one while using the other is not a
consistent position.

Two independent Shizuku tethering prototypes take this path, and one carries the
whole recipe:
[v2rayNG#5903](https://github.com/2dust/v2rayNG/pull/5903) assigns
`2001:db8:9877::1/64` to the TUN, with the comment that "Android only delegates a
/64 that it considers globally scoped" and that the documentation prefix
"satisfies that platform requirement without claiming a real public network".

Delegation needs an IPv6 default route and a global IPv6 address in the filtered
upstream properties, and DNS is not among the conditions
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#210)),
where "global" means the `isGlobalPreferred()` `/64` above. So the TUN needs that
`/64` and an IPv6 default route, both of which
[Link Properties](#link-properties) already requires.

There is a third condition, and it is a platform limitation rather than something
to satisfy. The same method reaches those checks only for
`mActiveDownstreams.peek()`, and AOSP says plainly that it "only support[s]
tethering IPv6 toward the oldest (first requested) active downstream". So *at most*
one tethered interface receives the delegated `/64`, and the rest are IPv4-only.

At most, not exactly one, because a local-only downstream can hold that position
without using the prefix. `getInterfaceIPv6LinkProperties()` answers a
`STATE_LOCAL_ONLY` downstream from an early branch with its own ULA config and
never consults the upstream
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#191)),
yet it still occupies the queue head if it started first. Every tethered interface
then fails the `peek()` equality and no client gets IPv6 at all. This app's own
local-only hotspot can produce that ordering exactly like any other app's: it is a
root-mode feature that runs independently of this one, and neither stops, delays or
consults the other, so a local-only downstream started first sits at the queue head
while this mode's upstream is published beside it.

This mode does not restrict downstreams to work around either case, since the
restriction would be more disruptive than the limitation. Document both as
user-visible behavior, and qualify IPv6 with two tethered downstreams and with a
local-only downstream started first.

The upstream DNS list does not gate advertisement. Android synthesizes the RDNSS
it gives clients from the delegated prefix itself, so the ULA resolver address is
there to give proxied DNS an upstream target, not to unlock delegation. Test the
two separately: a delegation failure and a resolution failure have different
causes and would otherwise be diagnosed as one.

Both prototypes publish through `setupTestNetwork`, so they demonstrate
delegation on an *unrestricted* network. Prefix delegation is driven by upstream
`LinkProperties` rather than by capabilities, so it should carry over to the
restricted agent unchanged, but that is an assumption this design must qualify on
device rather than inherit. Confirming it is the first IPv6 milestone.

## Startup And Commit

Startup is one ordered transaction:

1. Verify Shizuku authorization and pin the Binder epoch, effective UID, and
   permissions.
2. Initialize hidden-API access, resolve all runtime members, resolve the
   ordinary connectivity manager as the initialization template, create the
   private context with its effective-UID attribution, build the private wrapped
   manager without a constructor, and prove its field and `sInstance` readbacks
   before mutation.
3. Register the ordinary app-UID tethering observer and obtain an atomic initial
   downstream/upstream snapshot.
4. Select the target VPN/default `Network` and reject recursive selection of the
   session TestNetwork.
5. Launch `vpnhotspotd` directly under the app UID, authenticate its private
   Unix control socket, and start the one launch-through-READY deadline.
6. Use the wrapped `TestNetworkManager` to create only the TUN. Keep the original
   PFD in the app process.
7. Build the immutable MTU-1500 addresses, routes, DNS, virtual-address sets, and
   Rust configuration; transfer a duplicate TUN FD with `SCM_RIGHTS`.
8. Rust performs only local socket creation, network binding, nonblocking option
   setup, accounting initialization, and TUN readiness checks. READY performs no
   external reachability probe.
9. Register the exact foreground request.
10. Acquire the tethering connector, pin that exact binder, and link a death
    recipient to it that reports into the ordered owner. Do this before the
    preference mutation, because connector death is what silently undoes it.
    Rollback and normal stop both unlink the recipient and drop the pinned binder;
    a session never leaves one linked behind.
11. Set `preferTestNetworks=true` through that connector and require a
    `TETHER_ERROR_NO_ERROR` result.
12. Register and connect the restricted agent. Require both
    `onNetworkCreated` and request `onAvailable` for the exact returned
    `Network`, then validate capabilities and link properties.
13. Commit one state based on the current global upstream observation.

| State | Meaning |
| --- | --- |
| `ARMED` | Tethering names no upstream; resources are prepared |
| `VERIFYING` | Tethering names an upstream that cannot currently be classified |
| `ACTIVE` | Tethering reports the exact owned TestNetwork |
| `RESTART_REQUIRED` | Tethering still reports an ordinary upstream |
| `STOPPING` | Admission is closed and ordered cleanup is running |

Only `ACTIVE` admits new dataplane traffic. A different TestNetwork is a
terminal collision. Until commit, any failure rolls back every completed step.

Closing admission is not enough when `ACTIVE` is lost, and the handover sweep is
not the right instrument either. That sweep is scoped to the egress layer and
deliberately preserves reassembly contexts, virtual-DNS transports, and
everything else keyed by TUN-visible tuples. Those tuples are Android's inner-NAT
output, so what breaks here is not which upstream sockets are bound but whether a
TUN-visible tuple still means the same client. Established state would then match
work from before the loss against a client that acquired that tuple after Android
rebuilt its NAT.

So the design carries two independent axes. `upstream_generation` says which
`Network` upstream sockets are bound to and drives the handover sweep. A
**downstream epoch** says whether TUN-visible tuples still identify the same
clients, and everything keyed by such a tuple carries it: TCP flows, UDP
mappings, Echo sessions, reassembly contexts, virtual-DNS transports, and
`platform_ipv4` principal membership. Advancing the epoch retires all of it. The
two move independently: a VPN handover advances the generation only, and losing
`ACTIVE` advances the epoch only, since `upstream_generation` does not change
when tethering leaves and rejoins the same `Network`.

Advance the epoch on any observation that can break the correspondence. This mode is global
rather than per-downstream, so all of them are the same observation seen from different sides -
tethering no longer naming the exact `Network` this session published:

- tethering reporting an upstream that is not this session's TestNetwork;
- tethering reporting no upstream at all;
- any loss of positive confirmation, `VERIFYING` included. Continuity has to be
  established, not assumed from a short absence: the Tethering service can
  restart and rebuild forwarding and conntrack while this session's `Network`
  handle never changes, so seeing the same handle again proves nothing about the
  NAT behind it. An earlier draft preserved state when the gap closed within
  `control_result_deadline`; that made a control timeout stand in for a
  NAT-continuity test it cannot perform. Advancing costs clients their
  connections on an observer hiccup, which is the cheaper error and one fewer
  mechanism;
Tethering connector death is the exception: it ends the session rather than
advancing the epoch. Nothing surfaces it for free, so startup installs the
observation as its own step: retain the exact `ITetheringConnector` binder used for
the preference and link a death recipient to it, delivering that death into the
same ordered owner as every other event. Without it the controller stays `ACTIVE`
across a network stack crash, after which tethering has reset
`preferTestNetworks` and reselected an ordinary upstream while this session still
believes it owns the path. `TetheringManagerCompat.eventFlow` cannot stand in for
it: it forwards callback events, and the `binderCallbackFlow` behind it installs
no death recipient, so it neither reports the death nor terminates.

Recovery needs a new app process, not just a new session. `TetheringManager`
caches its connector permanently and AOSP states that after a network stack crash
"no recovery is possible"
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#467)),
and `Services.tethering` is a process-lazy singleton, so reapply in the same
process recollects `eventFlow` against the dead binder and cannot obtain the
initial snapshot startup requires. Constructing a fresh manager around a new
binder supplier is the alternative, and it is not worth building for a case the
platform calls unrecoverable and the same comment says usually crashes the system.
Report it as requiring an app restart, and say so rather than offering a reapply
that will fail.

The app observes these, and Rust owns every item they retire, so the epoch needs a
transport rather than only a definition. Carry it as its own `SessionConfig` field
beside `upstream_generation`, and have the daemon acknowledge both axes it has
applied, not just the upstream generation.

The epoch says what to retire, not whether to serve, so carry admission in the
config too. Only the app knows the session is `ACTIVE`, and Rust owns the TUN, so
without that field the daemon has to guess: reopening on the acknowledgement
breaks the `ACTIVE`-only rule, and staying closed leaves no resume signal, which
coalescing makes worse because a repeated config is not a distinct event. With
admission as a field, both axes are level-triggered and the newest config is the
whole truth - retire this epoch, and serve or do not. Admission reopens when a
config says so and the daemon has acknowledged it, never on the app's observation
alone, since that would let a later `ACTIVE` reuse tuple state Rust has not yet
retired.

Returning to `ACTIVE` therefore always builds fresh state under the new epoch.
Nothing is migrated, so nothing needs reconciling.

The epoch does not fence the TUN, and it is not worth trying to. Packets Android
wrote before the change are already queued in the kernel and carry no epoch, so a
read after the advance can stamp an old client's packet with the new epoch. Do not
attempt a read-side barrier: a packet arriving indistinguishably late is admitted
either way, so it narrows a window it cannot close while adding a quiesce to every
epoch change.

The producers on this side of the boundary are a different matter, because they
are the daemon's own. A one-time purge of the writer queue is not a fence: an
old-epoch task on another worker can enqueue after the purge and its packet leaves
once admission reopens. Before acknowledging retirement, cancel and join the
old-epoch producers the same way a handover quiesces its send paths, and gate both
writer enqueue and dequeue on the current epoch so anything that still slips
through is dropped rather than sent. Resolver completions cannot be cancelled and
so must be gated rather than joined. That the ingress race is unclosable does not
make this one unavoidable.

That residual is accepted for both families, because each is one shared principal
whose members are explicitly not isolated from each other. Earlier drafts treated
it as an open IPv6 question, on the assumption that direct IPv6 clients would get
per-client principals; they do not, so a queued packet attributed to the wrong
client is indistinguishable from every other reason attribution is untrustworthy
here. Nothing left to qualify.

Use independent 60-second `control_result_deadline` values for Shizuku
authorization, the initial tethering snapshot, each preference result,
publication callbacks, destruction callbacks, request release, and cleanup-only
Binder acquisition. Tag callbacks with session and operation generations so
late results cannot commit or resurrect state.

Generation tags cover callbacks. They do not cover what a synchronous Binder call
returns, and `Binder.transact` cannot be cancelled, so `requestNetwork`,
`NetworkAgent.register`, and `createTunInterface` need ownership rather than a
timeout. A deadline cannot fire on the lane that is blocked in the call, and
offloading it only moves the problem: the call can return a live request, agent,
or PFD after rollback has already run, with nothing holding the result and
nothing left to release it.

Give every such call an owned run-to-completion record. Expiry marks it
abandoned and lets the owner proceed, but the record outlives the deadline, and
whatever the call eventually returns is disposed through it - closing the PFD,
unregistering the agent, releasing the request - before the generation is
retired. A generation is not retired while any of its records are outstanding.

That record bounds this session's bookkeeping and nothing else, which matters most
for the request calls. `ConnectivityManager` holds the process-static `sCallbacks`
monitor across `mService.requestNetwork()`, and the same monitor guards callback
dispatch and handler creation
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#4760)).
A wrapped transaction that stalls therefore blocks every ordinary connectivity
callback in this process for its duration, `Upstreams` included, and letting the
owner proceed does not release it. Nothing here can: the lock belongs to the
framework and is held by the stalled thread.

Treat that as a property of using `ConnectivityManager` at all, and qualify how
long such a stall can actually last. A wrapped call to a dead Shizuku fails
promptly rather than hanging, so the exposure needs a live but wedged remote. If
qualification shows real stalls, the escape is to drop `ConnectivityManager` for
these two calls and drive `IConnectivityManager.requestNetwork()` and
`releaseNetworkRequest()` over the wrapped binder with the session's own
`Messenger` and callback bookkeeping, which touches no static state. That means
owning the callback message protocol and its version drift, so it is a response to
measurement, not a precaution.

### Selected-Network Replacement

A VPN reconnect or default handover changes which `Network` the daemon binds to.
It is not a transaction and acquires nothing: the daemon already receives network
handles in its session config, already selects per destination, and already binds
every upstream socket itself through `android_setsocknetwork`. The change is a
config update followed by a sweep of the sockets bound to the old handle.

Only the egress layer is network-scoped. Upstream sockets and resolver
submissions belong to one `Network`; the TUN-facing layer does not. Reassembly
contexts, the common packet writer, virtual-DNS client transports, principal
membership, and the derived descriptor and memory budgets all survive untouched.

The egress generation is not the handle. A `Network` handle is derived from its
netId, so it survives a `LinkProperties` change that can invalidate pinned state,
and a netId is eventually reused by an unrelated network, after which a stale tag
would match a current one. The session config already carries
`upstream_generation`, which the Kotlin side increments on upstream property
changes; the egress generation is that independent monotonic counter. State is
tagged with the generation, sockets bind with the handle, and neither substitutes
for the other.

The daemon owns the change. The app observes the selection, rejects recursive
selection of the session TestNetwork, and sends the current handle as a
session-config update. What an implementation must satisfy:

- **Coalesce, never queue.** One pending slot holds the latest update. Each is a
  `ReplaceSessionCommand` with its own completion, so a superseded one completes
  as superseded, carrying the generation actually applied; otherwise its caller
  expires at `control_result_deadline` and needlessly retires the dataplane of a
  session that was updated correctly. `Routing` already coalesces downstream
  updates through this shape.
- **Quiesce before publishing.** Cancel and join the old generation's send and
  receive paths before the new generation becomes current. Publishing first
  leaves a window where a task on another worker enters `send` after the swap,
  and no join retracts bytes the old `Network` already accepted. Closing
  descriptors is not a substitute: `close()` does not revoke a file reference a
  syscall already holds. Work admitted during the quiesce waits behind the owner,
  so nothing binds a generation being swept.
- **Sweep abortively.** Upstream TCP sockets close with `SO_LINGER` zero, because
  an ordinary `close()` keeps transmitting queued bytes, retransmissions, and the
  FIN over the old `Network`. If setting the option fails, report it and close
  anyway: the residue is a drained send queue on a network the session is leaving,
  which is not worth ending a working session over. UDP and ping sockets close
  directly.
- **Discard, then signal.** Bytes read from a swept socket are dropped rather
  than delivered, and non-terminal packets they produced are purged from the
  writer and refunded, though partly written fragments are not replayed. Each
  swept state then writes at most one terminal downstream packet before it is
  freed - a reset per TCP flow *that has a remote endpoint to send one to*,
  SERVFAIL per resolver query where it fits owned capacity, nothing for
  connectionless UDP and Echo - so clients fail fast. A flow still listening, or
  one whose socket the stack has already closed, has no remote endpoint: it is
  aborted and freed in silence and no reset is counted for it. These writes
  are TUN-facing and therefore outside the layer being swept. What holds no
  selected-network socket is not swept at all: a virtual-DNS transport keeps
  running across a generation change, and the SERVFAIL its old-generation answer
  becomes is delivered on that same connection under the current stamp.
- **An uncontrollable session ends; it is not nursed.** The daemon acknowledges
  the applied generation. A failed write, a missing acknowledgement, or a reported
  sweep failure means the app can no longer tell what the child is bound to, and
  it has no way to make the child retire anything: the retirement is daemon-side
  work, so naming it as the response would assert an outcome with no owner able to
  produce it. There is also nothing to retry on. `DaemonIpc.writeFrame` writes the
  length and the payload separately, so a mid-frame failure desynchronizes the
  stream, and the controller responds by completing every in-flight call with an
  `IOException` and closing the connection. So this is an explicit session
  failure, reported, with stop and reapply as the way back.
- **Session failure is what fences the child.** Ending the session already runs
  the ordered stop, including the process-exit escalation and its bounded wait for
  exit, so the child stops forwarding through machinery that exists rather than
  through a new quiescence protocol. The wait is what makes it a fence:
  `destroyForcibly()` alone signals without confirming, so withdrawing the agent
  or admitting a successor before exit is observed would leave the old child
  forwarding on the superseded handle. That is also why there is no in-session relaunch: recovering a wedged
  child in place would need forced exit, confirmed death, and per-launch child
  epochs to keep its late events out of its successor, all for a state nobody can
  reproduce on demand. One session has exactly one child, and the session
  generation stays sufficient to tag its events.
- **No egress is not a failure.** With no selectable `Network` the app sends no
  handle, upstream work fails per operation as on any bind failure, and the
  session resumes on the next handle. `fallback_network` stays unset; this mode
  admits one egress and never falls back.

Sweeping rather than draining is the point. The old `Network` is usually still
usable, so letting established work finish would keep downstream TCP, UDP, and
DNS egressing outside the new one for the full flow timers below, hours in the
established-TCP case. `STOPPING` may drain because the session's egress policy is
ending with it; a handover is that policy changing.

A handover never republishes the agent, the request, or the preference, and never
touches the TUN. Tethering reevaluates its own upstream on the same default
switch, so the committed tethering state is rechecked rather than assumed.

#### Handover Residues

Two kinds of traffic outlive a sweep. Both are irreducible under the app UID and
neither is a defect to design away.

Kernel control traffic. Once `SO_LINGER` zero destroys the TCB, any further peer
segment elicits another reset, so the close-time reset is not a per-flow bound; a
segment arriving before the close syscall is acknowledged normally; and a late
reply to a closed UDP port elicits ICMP unreachable. There is no netfilter access
to stop any of it. It is bounded by the peer's retransmission budget and the
kernel's rate limits, carries no downstream payload, and names no tuple the old
`Network` has not already seen, so it does not weaken the guarantee, which is
about downstream traffic.

Resolver work, the one part of a sweep the daemon cannot make synchronous. Treat
`android_res_cancel` as "stop reading the answer", not "stop the query": it
closes the proxy socket and does nothing else
([source](https://android.googlesource.com/platform/system/netd/+/refs/tags/android-17.0.0_r1/client/NetdClient.cpp#586)).
The resolver's only cancellation test is gated twice over, and this caller never
reaches it. It runs on the plaintext path exclusively when `attempt > 0`,
deliberately leaving the initial query unguarded
([source](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-17.0.0_r1/res_send.cpp#605)),
and its body returns early unless the `no_retry_after_cancel` experiment is
enabled, which defaults to disabled
([source](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-17.0.0_r1/res_send.cpp#441)).
The private-DNS path performs no cancellation test anywhere, including a
strict-mode wait that blocks for seconds
([source](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-17.0.0_r1/res_send.cpp#1440)).

`query_network` always passes `ANDROID_RESOLV_NO_RETRY`, which sets `retryTimes`
to one, so `attempt` never exceeds zero and the test is unreachable regardless of
the experiment. Cancelling stops nothing, but the residue is smaller than a
retrying caller's. Under this flag, and only when more than one server is usable,
the resolver picks a single server by query id and skips the rest
([source](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-17.0.0_r1/res_send.cpp#587)),
so the plaintext part is one query to one selected server rather than a walk of
every configured one, preceded by the private-DNS path when one is configured.

The flag bounds the outer attempt loop and nothing below it, so one attempt is
not one packet: an oversized query uses TCP from the start
([source](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-17.0.0_r1/res_send.cpp#597)),
truncation drives a TCP re-query, and that path carries its own transport retry.
Measure the private-DNS part, the plaintext datagram, and any TCP exchange
separately; changing the resolver flags changes all three.

Which queries leak needs no hold or acknowledgement, because the owner that
applies the handle also submits them: a query is either submitted before the swap
or bound to the new handle after it. What leaks is exactly the set already handed
to dnsproxyd, and one not yet transmitted when the swap lands puts its first
packet on the old `Network` afterwards, exposing a name that network never saw.

Cancelling recovers the daemon's descriptor and discards late results. It does
not recover the platform's capacity: `DnsProxyListener` holds a limiter slot
across `resolv_res_nsend`
([source](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-17.0.0_r1/DnsProxyListener.cpp#1110)).
The limits are `MAX_QUERIES_PER_UID = 256` and `MAX_QUERIES_IN_TOTAL = 2500`
([source](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-17.0.0_r1/OperationLimiter.h#32)),
and exhausting the per-UID one fails the next query with `-EBUSY` and "max
concurrent queries reached"
([source](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-17.0.0_r1/DnsProxyListener.cpp#1123)).

So do not cancel on handover at all. Cancelling stops nothing and destroys the
only thing that made the debt observable: with the descriptor closed there is no
completion to wait for, and the resolver's timeouts are configured at runtime, so
no source-backed deadline bounds the wait. Keep the descriptor, await EOF,
discard the answer by generation, and refund on completion, which makes the debt
exact for the cost of a descriptor the daemon already owns.

Across a session boundary that is impossible: death closes descriptors while the
platform keeps the slots, and no ceiling survives unbounded restart churn, since
each dead session leaves a pool draining. Half the limit fails for the same
reason in miniature, two pools of 128 consuming every slot with nothing left for
the app process's own resolver use. Size the ceiling as a measured small fraction
of `MAX_QUERIES_PER_UID`, leaving room for more than one draining predecessor and
for app headroom, and treat `-EBUSY` as a transient returning SERVFAIL under the
existing admission-denial rule rather than as an invariant violation. Sizing
makes it rare; handling it makes it harmless.

**Decided: an eighth, so 32.** Half would let two pools consume every slot with nothing
left for either; an eighth leaves seven pools' worth of headroom for draining
predecessors and for the app process's own resolver use. It is ample in absolute terms,
and checkably so rather than by assertion - the daemon reports its slowest resolver round
trip, measured at 274 ms under a deliberately saturating 80-query burst, so 32 in flight
sustains over a hundred queries per second even at that worst case. Exhaustion is not an
error: the query is refused with SERVFAIL, which is what that burst measured.

These hold across the supported range, not only Android 17. The single-server
selection, the 256-query per-UID limiter, and its `-EBUSY` refusal are present at
every `android-13/14/15/16/17.0.0_r1`. The per-UID constant moved from
`DnsProxyListener.cpp` into `OperationLimiter.h` in 15, `MAX_QUERIES_IN_TOTAL`
exists only from 15, and no cancellation test exists at all before 16. Re-verify
against the installed module rather than `SDK_INT`, since DnsResolver is
Mainline.

Measure the residue per release, including its duration and the private-DNS path,
and treat a growing one as a compatibility finding. Owning the resolver sockets
would remove it and is rejected: that would abandon platform private DNS,
caching, and the handoff [`dns.md`](dns.md) specifies, to close a window bounded
in seconds rather than in the flow timers below.

## App-UID Native Bootstrap

Launch the installed `vpnhotspotd` entry directly from the app process; do
not use a Shizuku UserService, root shell, persistent `app_process`, or JNI-hosted
packet engine.

The app creates a private Unix-domain listener and nonce. Rust connects and the
app verifies:

- peer PID equals the launched child;
- peer UID equals the app UID;
- nonce and protocol version match.

The TUN FD travels from the app to Rust, so its arrival is verified by the
receiver. Before READY, Rust rejects zero or multiple ancillary FDs and requires
the received descriptor to be a nonblocking TUN with the expected interface and
configuration. The sender cannot prove what arrived: it only sets the descriptor
nonblocking before duplicating it and keeps the original PFD.

Control-socket EOF is the authoritative Rust cancellation signal, though not an
acknowledgement of exit: the control loop cancels its calls and then joins every
task before stopping, so a task slow to observe cancellation keeps the process
alive. Drain stdout/stderr without blocking cleanup.

`destroyForcibly()` is not a force kill on Android and must not be used as one.
`UNIXProcess` does not override it, so the base implementation calls `destroy()`,
whose native side is `kill(pid, SIGTERM)`
([source](https://android.googlesource.com/platform/libcore/+/refs/tags/android-17.0.0_r1/ojluni/src/main/native/UNIXProcess_md.c#1056)).
Escalating from `destroy()` to `destroyForcibly()` therefore sends SIGTERM twice,
which does nothing for the wedged or signal-ignoring child the escalation exists
to handle, and the session would sit in `STOPPING` until its wait expired.

So shutdown is: wait 10 seconds, `destroy()` for SIGTERM, wait 5 more seconds,
then SIGKILL the child pid explicitly, then wait for observed exit. The pid is
already known from the control socket's peer credentials, and the app may signal
it because they share a UID. The final wait is what makes this a fence, since
everything downstream assumes the child is gone: the one-child invariant, the
claim that stopping fences it, and any successor that would inherit this TUN.
Stay in `STOPPING` and admit no successor until exit is observed. A child that
outlives SIGKILL past that bound is a structured report and a production blocker.

## Dataplane Semantics

All outbound sockets follow:

```text
create -> bind to selected Network -> verify options -> connect/send
```

Binding, source selection, option, connect, or send failure affects only that
operation. There is never a process-default or alternate-network fallback, and no
daemon-owned send or socket ever uses a retired generation, including work that
was in flight when it retired. Resolver transactions already handed to dnsproxyd
are the one exception, because their sockets belong to Android rather than to the
daemon; that residue is bounded and specified under Selected-Network
Replacement.

### Classification And Principals

State keyed below by "selected-network generation" carries both axes from
[Startup And Commit](#startup-and-commit): the upstream generation for what its
sockets are bound to, and the downstream epoch for what its TUN-visible tuple
means. Either advancing retires it.

Before attribution, reassembly, or transport dispatch, compare the destination
with the exact `session_virtual_addresses` set.

- An exact configured virtual-DNS TCP/UDP port-53 endpoint becomes
  `platform_dns`.
- A fragment for a virtual-DNS address is a provisional `platform_dns` candidate
  until its transport header is available.
- Every other protocol or port to a reserved address is dropped without a
  response or upstream socket.
- A destination that means something only on the link it arrived from - multicast,
  broadcast, link local, loopback, unspecified - is dropped without an upstream socket
  too. Re-originating one puts it on a foreign link, which leaks what the sender meant
  to keep local and asks the upstream to answer for a scope it is not in. In practice
  this is most of what a tethered link carries by volume: step 7 measured 26 such
  packets against 4 relayable ones in a quiet session, nearly all mDNS. Private and
  unique-local addresses are deliberately not in this set, since a VPN's own resolver
  is usually one of them.
- Other IPv4 traffic becomes `platform_ipv4`.
- Other IPv6 traffic becomes `platform_ipv6`, a second shared principal. Splitting
  it per client was the original design and is not possible: nothing distinguishes
  a tethered client's source address from one a local app chose.

Principals are therefore never client identities in this mode. There are exactly
three, all shared: `platform_dns`, `platform_ipv4`, and `platform_ipv6`. Two
consequences follow, and both are deliberate.

Classification still drives what a packet is *treated as* - which floor it may
reach, which owner handles it - but it no longer divides the budget. Accounting is
**self-protection, not fairness**, and it is one aggregate owner rather than a share
per principal or per family: what it bounds is the total the daemon can be made to
allocate, with a reserved floor inside that total for `platform_dns` alone, and it
cannot stop one client - or one local app posing as one - from consuming what is
available. The per-principal targets earlier drafts described are gone, not
reinterpreted; [Resource Policy](#resource-policy) is the authority on what
replaced them.

And per-MAC traffic accounting cannot be trusted to attribute upstream bytes to the
client that really sent them, since a local app can source packets from a client's
address. Per-client accounting and per-client blocking are simply unavailable in this
mode: both are root-mode features of this app - the per-client `iptables` rules
`routing/desired.rs` installs, and the app's own neighbour-driven routing - and
Android's system tethering supplies neither, so nothing takes them over.

### TCP

Key TCP state by family, TUN-visible source endpoint, destination endpoint, and
protocol, and record which selected-network generation opened it. Duplicate SYNs
reuse observable half-open state instead of allocating another flow.

**Two kinds of flow, and which one a flow is is recorded when it opens rather than
inferred from what it is doing later.** A relayed flow owns one selected-network
upstream socket and cannot exist without a selection, so the generation that opened
it is part of what it *is*: when that selection goes, so does the flow. A virtual-DNS
transport owns no upstream socket at all - its questions go to the platform resolver -
so a generation change leaves it running, with its mailbox and its one logical
resolver token, and its next question resolves on the successor selection. Inferring
the kind from a query being outstanding would reset an idle DNS transport, which is
the client this distinction exists to protect.

Apply the validated remaining SYN TTL/Hop Limit before connect. TCP is terminated,
so packet-by-packet hop-limit transparency and TCP traceroute are not promised.

**The client-side stack is seeded per session, from the kernel.** smoltcp's
`Config::random_seed` is zero unless it is set, and that seed is the whole state of
the RNG a passive open draws its initial sequence number from - so a
default-configured interface makes every session of this daemon issue the same
sequence of ISNs, and a client tuple reopened after a restart begins exactly where
its predecessor began.

What that risks is worth stating exactly, because none of the previous session's own
state survives a restart: the process exited, so its sockets and their TIME-WAIT
timers went with it. What can still arrive is what the *network* held - a delayed or
retransmitted segment of a connection this daemon terminated - and that is matched by
tuple and sequence alone. A successor beginning at the number its predecessor began
at is a connection whose window such a segment can land in, with nothing left here to
recognise it as old; and an ISN sequence that restarts identically every session is
one an on-path party can predict from having watched an earlier one. So one 64-bit
seed is read from `getrandom` before any *dataplane* owner or allocation exists, a
seed of zero is refused because zero is the default it would be indistinguishable
from, and a session whose seed cannot be read does not start. The control writer and
the reporter deliberately do exist by then, and that ordering is the point: it is what
lets the refusal reach the app as a structured report - naming the step and carrying
the errno - and be flushed, rather than arriving as an EOF on the control socket.
There is no fallback to a clock or a process id. Within a live session none of this changes
what owns `TIME-WAIT`: that is still smoltcp's own ten-second close timer, with no
outer floor of this daemon's - see [Timers](#timers).

Provide bounded bidirectional backpressure and half-close behavior. The
terminating engine must segment every packet so the complete TUN-side packet,
including options and extension headers, fits `test_network_mtu`.

Retire by axis rather than wholesale, because the two axes invalidate different
things. The epoch retires every flow, since each is keyed by TUN-visible endpoints
and those may name a different device now. The generation retires exactly the flows
that hold a socket bound to the network that changed - which a virtual-DNS transport
does not, so it is left running with its socket, its mailbox and its logical
resolver token. What a flow *is* has to be recorded when it is opened rather than
inferred from what it is doing: an idle DNS transport has no query outstanding, and
a kind read from the presence of one would reset the client this rule exists to
protect.

### UDP

Rust's outer UDP mapping is endpoint-independent and address-filtered:

- key by selected-network generation, family, and TUN-visible source address and
  port; destination is not part of the key;
- use one nonblocking unconnected upstream socket per mapping;
- pin one selected-network local address/port for the mapping lifetime;
- reuse that socket across destinations;
- record permitted remote addresses separately and accept replies only from
  permitted addresses;
- retire the mapping if its selected Network, pinned source, or socket is lost.

Carry the validated remaining TTL/Hop Limit in per-message ancillary data for
every send. Require received hop metadata for every reply; missing metadata drops
the reply.

A closed mapping's local identity outlives it. Inbound UDP and ICMP demultiplexing
matches on local address and port; the network mark that steers egress takes no part
in it. A late reply addressed to a retired mapping can therefore be delivered to
whatever socket now holds that address and port, acquire its generation, and reach
the TUN as data the receiving client never asked for.

One rule addresses it, and it applies to retired ping sockets and their Echo
identifiers as well as to UDP: **require the receiving interface index in ancillary
data on every reply, and drop replies that did not arrive on the current
generation's interface.** The metadata is one control message the daemon already
reads, so this costs nothing.

Earlier drafts also required quarantining a retired local port or Echo identifier
before reuse. That is removed. It would have forced the daemon to choose ports
explicitly rather than let the kernel assign them, which drags in a candidate walk,
an identity ledger, a budget for identities that hold no descriptor, expiry timers,
and a denial path when candidates run out - a new failure mode built to narrow an old
one. What remains after the interface check is a reply arriving on the same interface,
permitted by the new mapping's remote filter, for a port the kernel reissued, more
than the mapping lifetime late. The permitted-remote filter and the mapping timeout
already make that narrow, both mappings are the same shared principal that this mode
does not isolate anyway, and the harm is one bogus datagram. It is not worth a second
ownership protocol.

For IPv4, the mapping owner serializes `IP_MTU_DISCOVER` immediately before each
send:

- DF set -> `IP_PMTUDISC_DO`;
- DF clear -> `IP_PMTUDISC_OMIT`.

Never substitute another mode or allow another send/receive task to interleave
that mutation.

Remote ICMP correlation uses a separately byte-bounded chronological history.
Every record has its own absolute deadline that later traffic cannot refresh.
A unique match may translate one error; a match, expiry, eviction, ambiguity, or
untracked send permanently suppresses optional error translation for that
mapping generation and refunds its history. Payload forwarding continues.
History pressure evicts old records before it rejects payload.

### Virtual DNS

Only exact virtual-DNS TCP/UDP port-53 traffic enters the resolver handoff.
Every query, response, temporary buffer, and `android_res_nsend` result
descriptor is charged to `platform_dns`.

Use the selected-network resolver path. A query's handle and the config it belongs
to are fixed **per query, when the serialized ingress owner accepts that query** -
not per flow, and not when its transport first framed it. A request still queued
when a config wins is therefore published under the successor, and one the owner
has already accepted cannot be overtaken by that config's acknowledgement, because
both happen in the owner's own serial order. The pair is retained through
completion and is what settlement classifies the answer against, so an answer is
stale by generation even when the successor kept the same `Network` handle.

The client-facing transport is not network-scoped at all: a virtual
DNS exchange terminates locally and owns no selected-network socket, so a
handover neither retires its TUN-side UDP state nor resets its TUN-side TCP flow,
and a query submitted just after one still has a live transport for its reply.
A DNS-over-TCP transport therefore outlives the selection its earlier questions
went out on, and asks its next one on whatever is current then. For the same
reason it does not need a selected network to *open*: a connection accepted while
the session has none answers its questions with their own SERVFAIL and resolves
normally once a config supplies one.

On a stream, admit the length before the message exists. The two-byte prefix is
parsed first, the length it announces is charged - one DNS-class descriptor record
and exactly those bytes plus the answer allowance - and only a granted buffer of
exactly that length is filled. Nothing is copied, grown or handed to the platform
before that grant exists, since the largest message a client may announce is 65535
bytes and reading it first would hand that allocation to whoever asks. A framed
query the record floor has no room for is still answered: the second tier costs the
query, the SERVFAIL built from it and its framing, with no descriptor and no
platform slot. Only a query whose bytes do not fit either is skipped, and skipping
consumes exactly the announced bytes so the stream stays framed for the next
question.

Resolver binding failure never falls back to the default network. Bound
concurrent query descriptors with a nested global DNS ceiling sized under the
platform's per-UID limiter - see
[Selected-Network Replacement](#selected-network-replacement) for the chosen
fraction. Do not cancel a query to free ceiling capacity:
cancellation frees the daemon's descriptor and not the platform's work, and it
removes the completion signal that made the charge exact. Hold the descriptor,
await completion, and refund then; never refund on a deadline. Admission denial returns SERVFAIL only when it fits already
owned capacity; otherwise drop.

### ICMP

| Function | IPv4 | IPv6 | Rule |
| --- | --- | --- | --- |
| Echo request/reply | Ping socket | Ping socket | Optional independently per family |
| Destination unreachable | Supported mappings | Supported mappings | Translate only safely owned UDP/Echo state or generate locally |
| Fragmentation Needed / Packet Too Big | Supported | Supported | Preserve a validated MTU and require safe correlation |
| Time Exceeded | Supported | Supported | Preserve validated received hop metadata |
| Parameter Problem | Supported | Supported | Translate only when the pointer maps unambiguously |
| Reassembly timeout | Local context only | Local context only | Requires fragment zero and normal ICMP response rules |

Use one nonblocking ping socket per family and selected-network generation.
Translate kernel-visible Echo identifiers/sequences back to retained TUN-visible
state. Carry request TTL/Hop Limit per message and require received hop metadata
for replies.

Enable `IP_RECVERR`/`IPV6_RECVERR` for ping and UDP sockets. Validate family,
origin, type/code, offender, quote, MTU/pointer, and receive-hop controls before
translation. Never infer a UDP datagram from socket identity alone and never
numerically reinterpret an ICMPv6 header as ICMPv4; use separate family builders.

Remote errors are translated only for daemon-owned UDP/Echo state. Locally
generate family-correct errors for validated TTL expiry, route failure,
unsupported protocol, parser rejection, path-MTU failure, or reassembly expiry
when protocol rules allow it. TCP connect failure remains TCP behavior.

Do not handle Router Advertisement/Solicitation, Neighbor Discovery, DHCP, ARP,
or ICMP Router Discovery in Rust; Android tethering owns downstream link control.

### MTU, Output, And Fragments

`test_network_mtu = 1500` is immutable in agent `LinkProperties` and Rust.
Tethering clamps the downstream IPv6 MTU it derives from its upstream to the
1280-1500 range
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/android/net/ip/IpServer.java#894)),
so 1500 is the top of what the platform will propagate and matches the physical
downstream link. 1280, the IPv6 minimum, is the other end of that range: it would
remove almost all path-MTU signalling toward clients at a standing cost of
roughly fifteen percent per-packet overhead on every flow. Take the throughput.

The selected network is usually smaller than the TUN, so the size mismatch moves
into the daemon instead of disappearing. Terminated TCP absorbs it for free,
since each side segments to its own MTU. Relayed traffic does not, which makes
path-MTU signalling toward clients load-bearing rather than optional: derive it
from `EMSGSIZE` on a DF-set send and from the error queues this design already
requires, not from a configured upstream MTU that can change under a handover.

Newly originated TUN-side TCP, virtual-DNS, and local ICMP packets use immutable
`local_origin_hop_limit = 64`. Relayed UDP/Echo traffic and translated ICMP
errors preserve validated received hop metadata and never substitute that local
default.

- TCP segments to fit.
- IPv6 UDP, Echo Reply, and UDP virtual-DNS responses source-fragment in Rust
  when needed.
- ICMPv6 errors truncate their quote and never fragment.
- IPv4 TCP and errors stay within their limits.
- IPv4 packets up to the downstream floor are atomic with DF set. Fixing the floor at the
  IPv4 minimum reassembly buffer of 576 would be conservative in the wrong direction,
  because it would push almost all relayed traffic above the floor and so maximise
  fragmentation while discarding the path-MTU signalling this section calls load-bearing.
  The other candidate was a measurement over the tethered downstreams, which is what an
  earlier revision did, and which this mode may no longer do at all: it owns no downstream
  and does not look at one.

  **Decided, and this is the trade the global path makes.** The floor is
  `test_network_mtu`, carried to the daemon as `ShizukuSessionConfig.downstream_mtu_floor`
  and constant for the life of a session. This mode owns no downstream: it publishes one
  upstream and never reads, stores or makes policy from the interfaces Android happens to be
  serving behind it, so there is nothing narrower to measure and nothing that could move it.
  It is also the same number tethering clamps the downstream MTU it derives from this
  upstream to, so the two agree by construction on any downstream that takes 1500.

  What it costs is the one case where a downstream link is narrower than that. A relayed
  IPv4 packet above that link's MTU with DF set is dropped by Android's forwarding path,
  which answers ICMP Fragmentation Needed back at the TUN - the very signal this section
  already calls load-bearing - so the failure is a signalled path-MTU event at the cost of
  one packet and a round trip, not a silent black hole. A measured floor would have avoided
  that packet; it would also have made the floor a value that moves within a session, which
  is what the epoch rule below exists for.

  **The floor moves only with the downstream epoch**, and the daemon refuses a config where it
  moves without one: the floor is what every already-queued packet was sized against, so a floor
  that changed while the queue survived leaves packets built for a limit that is gone. With a
  constant floor that rule is never exercised, and it is kept because it is the daemon's contract
  rather than this mode's convention. The daemon still clamps the floor to the interface MTU,
  because a downstream wider than the TUN cannot rescue a packet the TUN will not carry.
- Only oversized IPv4 UDP, Echo Reply, and UDP virtual-DNS output may clear DF,
  use a guarded Identification, and rely on Android downstream fragmentation.
- Oversized DF-set output fails with the correct error/drop behavior.
- **There are two size limits, not one, and the floor is only the first.** Android
  fragments what it forwards, so a packet above the floor but within the interface is
  handed over whole with DF clear. A packet above the *interface* is a different case
  that nothing downstream can rescue, because the interface is what the daemon writes
  through - and it is reachable in practice, since a UDP reply to a client's own query
  to a real resolver can be several kilobytes. Both families therefore source-fragment
  there, IPv4 included, reusing the datagram's one guarded Identification across its
  fragments so they reassemble into one datagram rather than several.

All TUN writes pass through one common packet writer with bounded queueing,
atomic packet writes, final size validation, IPv4 Identification policy, and
IPv6 fragmentation. A partial multi-fragment write never replays the datagram or
synthesizes an error for the unsent remainder.

**Which owner does what is worth stating exactly, because it is easy to read that sentence
the other way round.** The size policy, the Identification decision and source fragmentation
all happen in the ingress task's output owner, *before* anything is queued: it compares
against the floor, asks the allocator for a value, hands that value to whichever producer is
building the datagram, and splits the result if the interface cannot carry it whole. What the
writer task does is dequeue, check the retirement stamp, validate the finished bytes one last
time, and write. It builds nothing and identifies nothing. The one thing it does own is the
answer to "did this reach the wire, and when" - see
[The Nonreuse Window](#the-nonreuse-window).

Two things that sentence leaves out, both of which an implementation has to answer:

- **There are two independent backpressure sources, not one.** The daemon's own
  queue filling is an admission decision, subject to the budgets above. The kernel's
  TUN queue filling is `EAGAIN` on a nonblocking descriptor, which is not an
  admission decision and must not be treated as one: it is a wait for writability,
  and a packet already accepted into the writer is not re-admitted or re-charged when
  that wait ends. Conflating them either drops packets the daemon promised to send or
  lets the queue grow past its budget.
- **"Partial write" can only mean a partial *datagram*, never a partial packet.** A
  TUN descriptor delivers one write as one packet or fails; there is no short write to
  resume. So the only partial case is a multi-fragment datagram whose later fragments
  fail, which is exactly the case the rule above covers.

Ingress IPv4 and IPv6 reassembly is bounded by family-valid overlap, extension,
length, and timeout rules. Reject overlapping/inconsistent fragments. IPv6
extension-header parsing is bounded, rejects forbidden chains, and treats atomic
fragments according to normal upper-layer parsing.

### The Nonreuse Window

The receiver-facing invariant is one sentence: the same `(source, destination, protocol,
Identification)` must never reach the TUN wire twice inside the sixty seconds a downstream may
hold fragments of one datagram. Everything below is what it takes to mean that, and each piece
closes something a per-tuple counter alone does not.

**A sequence ends rather than wraps.** Each tuple hands out all 65,536 values exactly once. The
65,537th datagram is not given a value that has wrapped round; it is denied, quietly and
counted, until the tuple may start again. A `wrapping_add` would have handed the next datagram a
value the previous one might still be out there carrying.

**Issuing is a transaction, because the value has to be chosen before the header exists.** It goes
*inside* the header, so it cannot be decided afterwards - but a datagram that then fails to build,
or whose every packet the writer's queue refuses, has put nothing carrying that value anywhere.
Treating those as spent is receiver-safe and bad for the sender: 65,536 attempts that never reach
the wire would deny a tuple its oversized output, and a client that keeps the queue full can
arrange exactly that against someone else's traffic. So the position is handed back when, and only
when, nothing at all was accepted. A fragmented datagram of which even one fragment was accepted
keeps its value, because packets carrying it exist.

**Allocation time is not wire time, so the window is driven from the writer.** A packet accepted
into the writer's queue may sit there, or sit parked waiting for the kernel to accept a write,
for as long as the kernel likes - the section above insists that wait is not an admission
decision, which is exactly what makes it unbounded. So the moment an Identification was *issued*
says nothing about when it became visible to a receiver. While the session continues, every guarded
packet the writer accepts therefore ends in one settlement back to the ingress task: written,
carrying the instant the write returned, or unwritten. A stale dequeue, a final-validation refusal and a retirement that
preempted a blocked write are all unwritten, and unwritten starts no window at all, because
there is nothing out there to collide with.

That holds **while the session continues**, which is the honest form of it. The endings of the
session itself - a fatal write, a settlement path whose consumer is gone, a cancellation - stop the
writer with packets possibly still queued, and those registrations are never settled. Nothing is
lost by it: the allocator is about to be dropped, and what covers whatever those packets did on the
wire is the successor session's opening window below. A write that returns neither the whole packet
nor an error is one of those fatal endings rather than a success, because a descriptor that takes
part of a packet has broken the premise this whole path rests on - the bytes out there are a
truncated packet, and calling that a wire use with a timestamp would be recording a datagram that
was never sent.

**A sequence may start again only when nothing of it is unaccounted for.** Two conditions, both
required: no packet of that tuple is still owed a settlement, and at least sixty seconds have
passed since the *latest* one that reached the wire. A datagram that fragmented locally has one
Identification across all its fragments and is settled once per accepted fragment, so a
partially admitted datagram waits for exactly the fragments the writer took and no others, in
whatever order they come back.

**A new session denies everything guarded for its first sixty seconds.** This is the one thing
per-tuple state cannot see. A daemon handed a new TUN a second after the last one stopped starts
every sequence from the beginning, and a receiver may still be holding what its predecessor
wrote. Sixty seconds of denial is exactly as long as that can be true. It is a *guarded*
denial and nothing else: IPv4 output within the downstream floor still goes out atomic with DF
set, IPv6 still source-fragments under its own 32-bit sequence, and the terminating TCP engine's
own packets are unaffected. Like every other denial here it builds nothing, enqueues nothing,
reports nothing, and moves one counter.

**The allocator will not issue what it cannot follow to the wire.** Every guarded packet the
writer is given is in exactly one of three places - a queue slot, the writer's hand, or the
settlement channel - so the channel is as deep as the queue plus one, and the allocator refuses
to register more than that many unsettled packets at once. That is what lets the writer settle
without ever waiting: a writer parked on a full settlement channel could not reach its
retirement arm, and the ingress task waiting on that acknowledgement is the one thing that must
never be blocked behind feedback it is itself the consumer of. A settlement channel that
somehow filled anyway, or one whose consumer is gone, ends the session rather than losing an
ending and carrying on with a window it can no longer trust.

## Resource Policy

Do not choose arbitrary flow, descriptor, packet, or memory counts.

### Descriptors

After opening fixed control/runtime descriptors, derive:

```text
dataplane_descriptor_budget =
    soft_RLIMIT_NOFILE
    - fixed_or_global_fds
    - cleanup_reserve
    - reserved_not_yet_open_global_fds
```

Charge actual peak descriptor cost for TCP flows, UDP mappings, retained ping
sockets, and every in-flight resolver query. A UDP mapping owns one persistent
socket; filter and remote records own none. A handover needs no reserve of its
own: the sweep frees descriptors rather than doubling them.

Measured on the qualified device: the app process's soft `RLIMIT_NOFILE` is 32768,
which the daemon inherits, and `ip_local_port_range` is 32768-60999, or 28232
usable ports. So **the identity budget binds before the descriptor budget** for
everything that occupies a local port - every TCP flow and every UDP mapping - and
the descriptor budget only becomes the tighter one for state that holds a
descriptor without a port, which here is in-flight resolver queries. Size and test
against the identity ceiling first; a descriptor-only limit would never be reached.

Local identities are a finite resource of their own, and the measurement above is
why: with 28232 usable ports against 32768 descriptors, the number of concurrent
mappings and flows is bounded by the ephemeral range, not by the descriptor budget.
Deny a new mapping when that range is exhausted, the same way a full descriptor
budget does. Nothing is held in reserve beyond what is in use, because retired
identities are returned to the kernel immediately.

That measurement was taken from a root shell, and **the daemon cannot repeat it**:
`/proc/sys/net/ipv4/ip_local_port_range` is not readable at the app UID, which SELinux
denies along with the rest of `proc_net`. It does not have to be. The identity ceiling
enforces itself - the kernel refuses to hand out a port that does not exist, and the
daemon turns that refusal into the same denial a full budget produces - so the number
above informs the design without being assumed by the code. A device that narrows the
range is then handled rather than mispredicted, which is strictly better than hardcoding
28232 and being wrong somewhere.

So what the daemon actually enforces is the descriptor total, measured at session
start: the soft `RLIMIT_NOFILE` it inherits, less `/proc/self/fd` counted once the
session owns its control socket and TUN. The DNS floor of 32 is **inside** that total
rather than subtracted from it - general work stops at `total - floor`, DNS-class work
may enter the floor, and neither passes the total - so the number reported as the total
is the total. Both are reported: the aggregate, and the smaller general-usable ceiling
after the floor. The floor is sized at what this daemon's DNS can hold at once rather
than at the platform's whole per-UID cap, because the rest of that cap is spent in other
processes whose descriptors are not this process's to reserve. Measured on the qualified
device: 32756 records from 32768 less 12 open, of which 32 are the DNS floor and 32724
are generally usable.

Records that hold no descriptor - a mapping's permitted remotes, most of all - are
charged one unit against that same total. That over-restricts them against a memory
ceiling this process cannot read, and it is deliberate: an unbounded remote set is a
memory hole any local app can drive, per [Security Posture](#security-posture), and a
conservative measured bound beats none.

A **table** is not charged in records. An IPv4 Identification entry is a sequence position, a
count of packets the writer has not settled and one timestamp, under a three-field tuple with
no descriptor and nothing worth a descriptor unit, so that table is charged once, as one owner,
for the rows it was prepared for - sizing it from the descriptor count made a table of counters
as expensive as the mappings and flows that count actually measures. What follows from that here
is that a reclaim costs no bytes and refunds none.

**What a prepared hash table is charged for is its row state, not its allocation.** The shared figure is
`logical_footprint::<Row>(rows)` - the rows a bound allows, times what one row is - and the row type is named
rather than a byte count, because a `HashMap<K, V>` stores `(K, V)` and `size_of::<K>() + size_of::<V>()` loses
whatever padding the pair has between its fields. The UDP relay's reply filter is `HashMap<IpAddr, Instant>`,
which sums to 33 where the pair is 40, so seven bytes a row went uncharged until the type was passed instead of
the size. Passing the type makes that arithmetic unwriteable rather than corrected once, and a count whose cost
cannot be stated is refused rather than wrapped.

What the container allocates *around* those rows is deliberately not modelled. How many slots it takes for a
requested capacity, what it keeps beside them to find a key, and when it reorganises are the container's own
business, and `std` documents none of it - so standard hash backing sits in the same **count-bounded** category
as the async runtime's task cells: what the daemon states is how many rows may exist, which it enforces, and
what those rows cost as state, which it can compute. An earlier draft instead derived an exact byte figure
from one compiler's internal hash-table layout, and pinned that compiler at the repository root to hold it
still. Both are gone. A figure that is exact only against private behaviour is a figure that goes wrong
silently, and the honest version is the one that says what it leaves out: **the byte total is not RSS**,
and for a table-shaped owner the real allocation exceeds the charge by whatever that overhead is. The IPv4
Identification table is the one owner where the uncharged term dominates rather than rounding beside real
buffers, and what is bounded there is its *cardinality*: the sixteenth of the dataplane's measured share it is
solved against is a policy budget for charged row state, which fixes how many tuples may exist at once. The
backing those rows sit in scales with that count and is deliberately left unquantified - the solver is not a
proof about how many bytes the container really took.

Everything the daemon *can* see stays byte-charged in full: payloads, every `Vec` and `VecDeque` - `Vec`
guarantees one contiguous allocation of exactly `capacity` elements - channel allocations and the payloads
their slots may carry, per-worker scratch, ancillary buffers and reassembly buffers. Logical row state is the
only charge that is a policy figure rather than a measurement, and each charge is taken exactly once: the fixed
table owns its rows, and a live row's own lease owns only what that row allocated *beyond* the row - a
mapping's reply filter and history deque, a reassembly context's payload and range list.

**A new row needs a free slot in the owner's logical maximum, and nothing else.** Every table that fills and
erases has one explicit maximum row count - the number its charge was computed from - and that is the whole
admission condition, alongside whatever real resource the row also costs (a descriptor, a record, a DNS token).
The admission ledger, both maps of each worker registry, the fair queue's flow map, a UDP mapping's reply
filter, the ingress reassembly contexts, the DNS transaction rows, the Echo session table and the IPv4
Identification table all work that way, and each refuses *before* it commits to anything a refusal would have
to unwind. The fair queue's readiness marker is the one thing that cannot be gated, because a wake arrives
after the owner already owns a payload and there is no honest way to refuse one; it is therefore a flag on the
flow's own row rather than a second collection that could run out of room and strand bytes.

Removal is ordinary. A mapping expiring, a worker retiring, a context completing, a transaction settling, a
tuple being reclaimed: each frees a logical slot, and the next arrival may have it. Nothing consults what the
container has done with its own backing, because under this policy that backing is not accounted state - it is
opaque count-bounded overhead, and the container is free to reorganise or reallocate it whenever it likes,
including a temporary peak while it does. `with_capacity(maximum)` is how every one of these is built, so the
common case allocates nothing, but that is an ordinary initial reservation and not a correctness oracle.

What the daemon promises here is the **cardinality** bound: how many rows of each kind may exist at
once, enforced, with row state charged for exactly that many. It does not promise a byte figure for container
backing, it is not process RSS, and it makes no claim about how a container behaves under pathological
insert-and-remove patterns. That is the well-behaved-client assumption stated plainly - this is a local daemon
serving apps on the same device, not a defence against an adversary who controls allocation patterns - and the
logical cap is what keeps a misbehaving app bounded rather than unbounded.

What holds it in place is a test beside each owner, in two shapes. One is an equation: the charge for a bound
is exactly its rows times one row plus whatever contiguous storage that owner also holds, so a term that went
missing or got counted twice fails there rather than in a figure somebody reads. The other is behavioural: the
bound admits its whole count, the next one is refused atomically with nothing half-done, and a row removed is a
row the successor takes - repeated, because that is the shape a long session is in. No test reads
`HashMap::capacity`.

The UDP reply filter is proved in two halves rather than one, honestly: a host test cannot reach the
constructor at all, because `Relay::open` builds its mapping only past a real `egress::open_udp` and the
Android network-selection call is a stub that refuses in a test build. So one half asserts the production
charge - `Mapping::footprint`, which is what would undercharge if a term went missing - and the other drives a
real `HashMap<IpAddr, Instant>` through the filter's own permit-and-expire pattern at the same bound. What is
not covered is the constructor's single call, which review and the shared helper hold. Adding Android
behaviour to a host test to close that gap would be buying a proof by making the thing under test less like
production.

That table, when full, refuses the *new* tuple rather than evicting a live one, and the
datagram is dropped - quietly, counted, before any header is built. Not "no Identification,
so set DF": it is above the downstream floor precisely because something downstream is
expected to fragment it, so sending it atomic is a packet that downstream must refuse, and
that refusal is not a fact about the client's path but this table being full - which the
client cannot act on and would cache as though it could. **A denied datagram is dropped, not
sent atomic.** Quietly, because which tuples
arrive is traffic: repeated attempts coalesce into one counter and allocate, report and
enqueue nothing. A genuine caller-requested DF-too-large stays on its own classified path.
Eviction
looked safe because a re-added tuple restarts its sequence - but restarting is exactly the
reuse the allocator exists to prevent: a sender with a live fragmented datagram in flight
gets an Identification it may still have out, inside the window a receiver reassembles
over, and the two mis-splice. Which tuple was evicted was chosen by whoever sent the most,
so an app could force the restart onto someone else's traffic.

What the table *may* do, and does, is give away a bucket whose occupant can no longer collide
with anything: no packet of it outstanding, and its latest successful write at least 60
seconds old. That is the same test a spent sequence has to pass before it restarts, because
they are the same question - a reclaim hands the bucket to a newcomer whose own sequence then
starts at the beginning of the same value space. A full table scans for those at most once per
60 seconds, so newcomers arriving in between cost one lookup and one counter each rather than
a whole-table walk an app can drive.

Earlier drafts divided the budget into non-borrowable per-principal targets with
deterministic remainder distribution, recomputed as membership changed, plus
grandfathering and over-target denial. That machinery existed for a dynamic set of
per-client principals. There are now exactly three principals, fixed for the
session's life, so membership never changes, there is no remainder to distribute,
and a static three-way split would only stop an IPv4-only deployment from using the
IPv6 third. It is removed rather than reduced.

What replaces it is smaller and does the one job left, which is self-protection:

- one aggregate owner, enforced for every admission;
- inside it, a reserved floor for `platform_dns` alone, because DNS state is small,
  essential, and would otherwise be crowded out by a flood of forged sources - which
  any local app can produce, per [Security Posture](#security-posture);
- no reserve for `platform_ipv4` against `platform_ipv6` or vice versa. Nothing is
  being protected from anything there: both are shared principals whose members are
  explicitly not isolated from each other, so a split would trade usable capacity for
  a guarantee this mode does not make;
- never evict established state to admit new work; deny the new work instead.

**Two totals, not one.** A descriptor is not a byte and neither substitutes for the
other: a UDP mapping costs one descriptor and a few hundred bytes, while a terminated TCP
flow costs one descriptor and two 64 KiB buffers. Counting the second as "one record" let
memory run out long before descriptors did, and counting the first in bytes would let a
flood of forged sources exhaust the process's descriptors while the byte total said there
was room. So every reserve names both, and a request that fits one and not the other is
denied in both.

**Grants are leases, and leases never refund themselves.** A grant is an inert identity;
only the admission owner mutates or releases what it stands for, and only after the state
it accounted for is actually gone - the worker joined, the record erased, the allocation
dropped. Workers never refund: every place a worker could do so runs *before* the thing
being accounted for is gone, and releasing on cancellation hands a budget to the work that
will race the task still holding it. A lease dropped without being released therefore
leaks its capacity for the rest of the session, which is fail-closed rather than fail-open
and is visible in the session's closing line as an outstanding grant nobody released.

**Reserved before it exists, released after it is gone, and both on one path.** The rule above is
about *when* a lease may be released; this is about there being a single place that decides. The
session's fixed owners - the writer's three channels, the ingress read buffer and packetization
peak, the reassembly table, the Identification table - are reserved and then built by one call, in
that order, and that call is the only production path to any of them: nothing else in the daemon
constructs a writer or a queue, so nothing else can hold one it has not paid for.

That is a reviewed property of one function, not a compiler-enforced one, and the difference is
worth stating rather than glossing. The writer's channel constructor stays reachable within the
crate because sibling test harnesses build a writer without an aggregate, so the type system does
not forbid a second caller - what forbids it is that there is exactly one, it reads in order, and
the bundle it returns is what every production consumer takes its writer from. A compile-time
token was considered and rejected: it would add an argument that exists only to be unforgeable,
and churn every test harness, to prevent a mistake no production code is positioned to make.
Teardown runs the reverse, and every exit funnels through it, including a failure to start the
traffic owners: the owners stop, join and refund, then the output owner is dropped, which takes the
Identification table and the last of the writer's senders with it, which is what ends the egress
task. The egress
task's last act is to close the settlement channel, so the ingress task waiting for that close is
waiting on proof that the queue, both endpoints and any packet the writer had in hand are really
gone - and only then are those bytes given back. The channel is one the writer's own reservation
already paid for, so nothing here needs a second handshake to say the same thing. Releasing sooner
would not be a moment's inaccuracy but the whole of a teardown spent describing allocations that
still exist.

**Prepared, not grown.** Every collection an admission can add to is allocated to a
prepared capacity and charged for it once; admitting inside that bound is ordinary, and
admitting past it is refused rather than grown. For a contiguous collection the bound *is*
the allocation, and `with_capacity` makes it real up front. For a hash table the bound is a
**logical maximum row count**, `with_capacity(maximum)` is an initial reservation so the
common case allocates nothing, and the container is left to manage its own backing from
there. Growth of the bound itself, where it happens at all, is a replacement transaction:
the new footprint is charged beside the old, the move runs, and only then is one of them
released - because during a rebuild both allocations exist, and charging only the larger of
them describes a moment that never happened.
Removal is the other half, and it is where the two kinds part company. A contiguous
collection - `Vec`, `VecDeque` - keeps the capacity it was given until it is asked to shrink,
so the bytes charged for it really are still held. For a hash table what is retained is the
*charge*, because it was taken for the bound rather than for the rows in it: nothing is
refunded when an entry leaves, the freed slot is simply available again, and no claim is made
about the container's own memory either way.

**Prepared capacities are solved, not picked, and solved against what *general* work may
have.** How many TCP flows the engine prepares its tables for is the largest count whose two
64 KiB stack buffers each, plus every table indexing them, fits the general headroom - the
general ceilings less what general work has already taken, and nothing else. "Total minus
charged" would count the reserved floor as though a relayed flow could reach it, so ordinary
traffic's prepared share would be inflated with capacity that exists so name resolution and
already-accepted packets are never crowded out. How many IPv4 Identification tuples the
output table holds is a documented share of the same headroom. A constant would be wrong in both
directions at once: too large on a device whose measured share could never afford that
many, so tables are charged for capacity nothing can reach, and too small on one that could
afford more. The same solved number sizes every table that indexes those flows, so no two
of them can disagree about what may exist.

**Every step before the commit point is undoable, and the commit point is the last fallible
one.** A UDP mapping's first datagram is the worked example. Both records and every byte are
one grant; the socket, the prepared reply filter, the prepared history and the receive
worker all exist before the send - the worker *retained and blocked on a gate it has not
been let through*, because a task spawned after a successful send is a fallible step after
the commit point and one spawned before it and left running would receive for a mapping
that was never published. After the send there is nothing that can fail and nothing that
allocates: publishing is three field writes and a signal. A precommit failure cancels the
worker and marks the record rolled back - it stops being a mapping at that instant, with no
reply matched to it, no remote permitted through it and no expiry deadline - and its
terminal settles it through the same join fence every other ending uses.

**Transient peaks are charged, not skipped.** Taking a fragment into a reassembly context is
not a step from one size to another: the old payload buffer is alive while its replacement
is filled, the range list is rebuilt beside itself, and a fragment that completes the
datagram has the assembled packet alive beside the payload it came from. The projection
names retained bytes and that peak separately, and the whole peak is granted before anything
is allocated.

Every replacement is built at *exactly* the projected capacity and beside the original -
never by growing the original first, because a `Vec` at capacity doubles, and for that
instant the real allocation would sit above what was granted. The completed packet is
assembled while its context is still charged and the charge released only afterwards; the
function that assembles it cannot get that wrong, because it does not release - it hands back
what is owed, so only its caller can, and only with the packet already in hand.

The reconciliation is checked rather than saturating, and fails closed: more allocated than
was reserved means the context is discarded, its whole reservation returned, and a counter
raised. Carrying on would be an undercharge that widens with every further fragment, by an
amount a client chooses.

### Memory

Derive:

```text
dataplane_memory_budget =
    process_memory_ceiling
    - fixed_or_global_bytes
    - cleanup_memory_reserve
    - reserved_not_yet_allocated_global_bytes
```

Account for every piece of traffic-driven state, and its temporary peak, in the
dimension that state can honestly be measured in: allocated capacity for TCP
buffers, UDP payloads and history, DNS work, reassembly, output packetization and
scratch; logical row state plus a cardinality bound for the tables that hold
mappings, remotes, Echo entries and IPv4 Identification guards; and the peak of a
replacement while both allocations exist.

What the daemon actually enforces is a **policy share**, not a process ceiling and not an
exact process RSS or allocator figure: an eighth of the kernel's own `MemAvailable`
estimate, read at session start because it is the only memory number this process can
honestly measure. Exceeding it would not fail; it is the figure the daemon holds itself
to. What it counts is the Rust-visible owned heap this daemon chooses the size of - the
`size_of` of owner records, the *capacity* of the contiguous collections they hold, the
row state a bounded table is prepared for, and the fixed reservations for queues and
scratch that exist whether or not anything is in them. The next section says exactly
which allocations that includes and which are bounded by count instead.
Allocator-private metadata - arena headers, size classes, per-thread caches,
fragmentation - is explicitly outside the model, because this process cannot see it and a
number that pretended to would be worse than one that says what it excludes.

#### What Is Byte-Charged And What Is Count-Bounded

The model is deliberately **hybrid**, in the way ordinary Linux networking daemons
are: exact byte accounting for the memory this daemon chooses the size of, and
count-bounded accounting for opaque runtime plumbing whose layout has no supported
byte bound. Both halves are stated so the total says what it is.

**Byte-charged**, exactly or by a stated conservative bound:

- daemon-owned payloads and packet buffers;
- TCP and socket buffers, and query, answer and framing storage;
- every contiguous app-managed collection's capacity - `Vec` and `VecDeque`, whose
  documented allocation is one block of exactly `capacity` elements;
- transient replacement, assembly and copy peaks;
- scratch buffers;
- every bounded channel - its daemon-chosen depth, its message storage, its shared
  state and its retained blocks - through the one `channel_footprint`, whose
  arithmetic is an upper bound on the *audited locked* runtime's layout for the
  targets this daemon is built for, and which has to be revalidated when that
  dependency moves rather than being a cross-version theorem;
- fixed and prepared table capacity, as the *row state* a bound allows: a hash table's
  rows are charged at `entries * size_of::<Row>()` and what the container allocates
  around them is count-bounded instead, below;
- all other memory whose cardinality or byte capacity this daemon chooses.

**Excluded from byte attribution and bounded by count instead:**

- opaque Tokio executor and task backing, including task cells and `JoinSet` list
  entries and their backing;
- standard hash-container backing beyond the row state charged above - how many slots a
  requested capacity takes, what is kept beside them to find a key, and when the container
  reorganises are all undocumented, so what is bounded is how many rows may exist;
- `CancellationToken` backing nodes;
- `oneshot` shared cells;
- opaque reactor registration backing, such as `AsyncFd`'s scheduled-I/O cell - see the split below,
  because only the *live logical registration* is the daemon's to count;
- fixed shared-wrapper backing tied to a charged descriptor or record, such as the
  allocation behind an application `Arc<AsyncFd<Socket>>`;
- equivalent fixed-cardinality runtime plumbing whose layout has no stable,
  supported upper bound.

This is **not** the allocator-private exclusion above; it is a separate, explicit
policy category. The difference from a channel is what makes it a category rather
than an excuse: a channel's cost is a function of a dimension this daemon picks -
a depth and a message type - so a conservative upper bound follows from what the
daemon itself chose, and the arithmetic is checked against the audited layout of
the locked runtime. A task cell, a cancellation node, a oneshot cell or a reactor
registration has no such dimension, and neither does a hash container's own
indexing.

What replaces a byte bound for the hash containers is a **cardinality** bound: each
one has an explicit logical maximum row count, that maximum is what its row-state
charge was computed from, and it is the one condition a new row is refused on. A row
removed frees a slot the next arrival may take. The container manages its own backing
and may reorganise or reallocate it whenever it likes, including a temporary peak
while it does; `with_capacity(maximum)` is how each is built, as an ordinary initial
reservation rather than a guarantee. So the honest statement is that the row count is
bounded and enforced, and the bytes behind it are not modelled - not that this
process cannot allocate behind the accounting's back.

Being exact about *why* each is excluded, because two of them are excluded by
choice rather than by necessity, and saying otherwise would be the kind of claim
this document exists to avoid:

- a **task cell** is a fixed shape for a given future, and a non-child
  `CancellationToken` node is fixed-layout too - its children vector grows only
  for child tokens, which this daemon never creates, and the `Notify` beside it
  keeps its waiters *intrusively*, inside the `Notified` futures, rather than
  allocating them. Both are excluded because their layouts are crate-private and
  therefore have no supported size to quote, not because they cannot be sized.
- a **oneshot** does have a type-sized one-slot cell - an `Option<T>` inline
  beside two waker slots - so it could in principle be charged. It is excluded as
  a deliberate policy choice: one per record is a hard structural limit the daemon
  enforces, the rest of the cell is crate-private, and charging half of a private
  structure would be a figure that looks exact and is not.
- a **reactor registration** is the one that genuinely cannot be bounded by this
  daemon at all, and it needs the split below.

So the honest bound for all of them is on how many can exist, not how large each
one is - with the reactor exception stated next rather than assumed away.

**The reactor split.** A live `AsyncFd` or `TcpStream` registration is the
daemon's own: one per descriptor it opened, counted by the charged record that
owns that descriptor. What is *not* the daemon's is what the runtime keeps after a
registration is dropped. In the locked Tokio, deregistering pushes a clone of the
registration's `ScheduledIo` into a runtime-global pending-release vector and the
real release waits for a later driver turn; the threshold that nudges the driver
awake is a wake heuristic, not a cardinality cap. The daemon neither sizes that
vector nor fences when it drains, so it belongs to excluded opaque runtime
infrastructure and must not be presented as part of the record-count proof.

One consequence is worth naming rather than hiding: a single logical TCP flow can
have two registrations' backing alive at once. The connect path registers the
socket briefly to wait for writability, drops that registration, and the flow then
registers the same descriptor as a `TcpStream`. The first registration's backing
can still be pending release while the second exists. That is correct and
deliberate - the temporary registration is what keeps "this process could not
register the descriptor" distinguishable from "the path refused the connection" -
and it is exactly why the count-bounded claim is scoped to *live logical
registrations* rather than to every reference the runtime still holds.

**The structural constraints that make the count a bound.** Every excluded
traffic-driven cell must satisfy all of these:

- its record or lease is reserved *before* the cell is constructed;
- its live logical count is bounded by the charged prepared records, plus at most
  one synchronous candidate held by an owner that cannot interleave another. What
  the *runtime* still references after the daemon has dropped its handle is not in
  this count - see the reactor split above;
- one admitted worker owns at most one logical task bundle and one fresh,
  non-child cancellation node, plus only the named oneshot gate or handoff its
  owner documents;
- no child-token tree, detached subtask, unbounded waiter or fan-out, and no
  second runtime cell may silently multiply within one charged record;
- normal teardown joins or drops the runtime owner *before* the record or lease is
  released. The one exception is a *refused* synchronous candidate, whose identity
  and token may drop after its lease has gone back - which is harmless precisely
  because the owner is synchronous and cannot begin another admission before it
  returns;
- whole-owner and session destruction may abort cells that outlive their record,
  and no further admission happens against a dying owner. How long such a cell
  survives is the runtime's business, not something this daemon fences;
- a future change that multiplies these cells must either supply a stable byte
  bound or come back for policy and design review.

Fixed per-session and per-engine runtime cells are a separate, smaller set from
the traffic-driven per-record ones: the ingress task, the writer task, the reporter
window task, the engine's own sweep token, and the session's control channels. They
do not scale with traffic and are named here rather than counted per packet.

This policy scopes the **Shizuku Admission-managed dataplane** and nothing else. It
makes no claim about root-mode or NAT66 token ownership, which this accounting does
not manage.

Capacity, not length, is the unit - and which capacity depends on the collection. For an
explicit contiguous buffer the charge *is* its allocated capacity: `Vec` documents one block
of exactly that many elements, so the bytes are held until a shrink, a growth replacement or
release says otherwise. For a hash table the charge is the row state of its **logical maximum
row count** - `entries * size_of::<Row>()` - and what is retained across a removal is that
charge, because it was never a statement about the container's backing. That backing is
outside byte attribution entirely, so nothing here asserts it persists, shrinks or stays any
particular size. Expiring an entry therefore refunds the entry's *record*, frees its logical
slot, and refunds bytes only when the owner itself says an allocation went.

One aggregate byte total with a reserved floor for essential work, for the same
reasons as the descriptor total above; per-principal byte targets between the two
traffic principals are removed for the same reason. The reserved floor is derived rather
than chosen: it is what the admission ledger's own rows cost, plus the headroom one
maximum resolver exchange and one output packetization peak need. A measured share too
small to hold that is a session that does not start, answered before any packet arrives -
not a stream of denials later blamed on traffic. Never evict established TCP,
UDP, Echo, or DNS state to admit new work.

Fixed bounded queues are charged once, and for what a channel really is rather than for
the messages in it: its reference-counted shared state, its value blocks and their headers,
and every payload its slots may carry. Charging only `depth * size_of(message)` understated
the allocation - and an understated bound is the fail-open case the whole aggregate exists
to prevent. The figure is an upper bound on the *audited locked* runtime's layout rather than
a reproduction of it: block width, header shape and block recycling are all crate-private, so
the constants are read off the pinned version for both target widths, are chosen at the
extreme of each axis so neither target can make them short, and have to be revalidated when
the runtime dependency moves. It is not a cross-version theorem and is not claimed as one.
The depth a charge is taken at is the depth the channel is built at - asserted where the two
meet rather than kept in one function, so an owner can reserve before it allocates - including
the minimum an owner still allocates when its derived bound came out at zero. Nothing is charged a second time per
item. The reply queues are a real allocation bound rather than
a nominal one, because a reply worker takes its slot *before* it sizes, reads or allocates
the datagram: at most `depth + 1` maximum-sized payloads exist across the whole relay - a
full queue plus the one the ingress owner has taken and is still working on, because taking a
message returns its permit before that message dies - however
many mappings exist. The alternative - reserving after allocating - leaves one fully
allocated payload parked in every worker whenever the queue is full, so the in-flight cost
becomes the number of mappings rather than the depth that was charged for. For the same
reason the payloads belong to the *queue's* reservation and not to each mapping: a maximum
payload charged per mapping is 64 KiB every mapping could never use, and the sum of those
is not a bound on anything. What a worker does own for its own life is charged with it: a
ping socket's persistent receive scratch, and each worker's fixed error-queue ancillary
buffer.

What comes before the slot is only the wait for readiness. A worker that reserved first and
waited afterwards would let idle workers hold every one of the few reply slots while a socket
with a datagram on it waited behind them, so the order is readiness, then the slot, then the
read, then the allocation - one turn, owned by one function the workers call rather than
assembled at each call site, because an order split across callers is one a caller can get
wrong.

The kernel-error path holds the slot it already took rather than giving it back and
reaching for another: one error is drained and sent through that slot, and if more remain
the socket stays error-readable and the next turn takes a fresh one. Releasing the slot and
then waiting would park an ancillary buffer outside the bound the slot represents.

Exactly one message per turn, whatever kind it is. Skipping past local refusals and
unattributable messages synchronously looked free - a syscall each - but how many of those
are queued is a remote's choice, and that loop ran between two await points, so a retirement
asking the worker to stop waited out the whole backlog first. One per turn puts a scheduling
boundary between every message; the queue still drains at one syscall per turn, because each
turn removes one. The send path's own error-queue drain borrows the ingress owner's single
scratch rather than building one: a fresh ancillary buffer per failed send is a second heap
allocation nobody charged, at whatever rate a client can make sends fail.

Output fragmentation hands each fragment to the writer as it is built, so at most one
exists beside the source packet. Collecting them first held a second copy of the whole
datagram, in as many allocations as the MTU divided into it - and how large the datagram is
is a remote's choice.

Before rejecting essential memory growth, reclaim in this order:

1. process due expiries;
2. evict the requester's oldest optional UDP error history;
3. evict globally oldest optional UDP history;
4. for non-reassembly work, retire the requester's oldest incomplete fragment
   contexts;
5. reject the triggering growth if it still does not fit.

Keep a nested incomplete-fragment ceiling inside the aggregate byte total - a *check*
within that total rather than a pool beside it, so reassembly and everything else cannot
between them promise more than the share the dataplane was granted - and reserve measured
headroom for one supported non-reassembly operation. Every context growth is preflighted
from a projection that bounds the real cost from over and then reconciled *downward* to
what was really allocated; an upward reconciliation would be an allocation that already
happened asking permission afterwards. Fragment pressure never evicts essential state and
never emits a timeout ICMP for resource-pressure eviction.

All reserve, commit, transfer, reclaim, and refund operations pass through one
admission owner and occur exactly once. Reclaim is the *owner's* to order and is never
hidden inside admission: a denial is a denial, and what to do about it - the ladder above -
is a decision the ingress owner makes.

**Transfers, not refund-and-reserve.** A logical resolver token moving from a closed
DNS-over-TCP transport to the question still in flight is one operation, because the
platform's slot is still taken: a refund followed by a fresh reserve leaves a moment where
the token looks free, and a second query admitted in that moment is one the platform's
limiter has no room for.

**A debt is owed for work that is happening.** An idle DNS-over-TCP connection owes its flow
buffers and its one logical token, and nothing else - not a query, not an answer, not a
framing copy, because it has asked nothing. A DNS-class descriptor record *and* every byte
that exchange will own are charged when a query is actually framed, at the length its own
prefix announces and before that message is stored anywhere: the announced length, not the
largest one a prefix could describe, and not after the bytes have been read. That is what makes a
transport closing over a question still in flight come out right: the flow's buffers go with
the flow, the token moves to the question, and the resolver still holds the query it will
still answer. Holding a descriptor record for the whole life of a transport that might never
ask anything was a debt for work that had not happened, taken from the floor that exists so
real resolver work is never crowded out. Each submitted query costs a descriptor record and
*no second token*, which is what keeps thirty-two token-holding connections from becoming
sixteen with a query each - an artifact of the accounting rather than a limit anyone chose.

**A slot Android holds is not a slot this process may reuse.** `android_res_nsend` is
irreversible, so a submission it accepted keeps a per-UID resolver slot whatever happens next
here. Two things can then leave nothing to observe that slot's end with: the wrapper around the
returned descriptor failing before there is anything to poll, and the readiness registration
being watched with going away afterwards. Either way the query's record and bytes are refunded
and its *logical token* is not: it is moved into a session-owned quarantine that only the
session ending releases. Refunding it would admit a second query against a limiter with no room
for one, which is precisely the `EBUSY` backlog the ceiling exists to avoid. A DNS-over-TCP
transport this happens to is reset, because it cannot ask again under a token that no longer
exists.

**Which of the three the submission was is carried as a type, not distilled into an error.** A
query the platform *refused* holds nothing of Android's, so nothing has to be quarantined for it;
one the platform *took* keeps a slot whatever happens next here. Collapsing the two into a single
failure on the way to the owner that acts on them is what quarantined a refused query's token, so
the distinction stays typed until that owner has read it.

*Whose* token that is differs by protocol, and the refused case is where the difference is easiest
to state wrongly. A UDP query owns its token outright - it is on that query's own grant, so a
refusal really does return it with the rest of that grant. A DNS-over-TCP query's debt owns no
token at all in the ordinary case: the token belongs to the *connection*, which keeps it between
questions, so a refusal returns the query's record and bytes and leaves the token exactly where it
was, on a transport that is free to ask again. The only token a TCP settlement can end or
quarantine is one a *closing* transport handed to that question's debt on its way out.

That quarantine costs no ledger row. A token is *moved onto* a grant its owner already holds -
its retained-table lease - rather than split into one of its own, because the ledger is derived
as one row per record-backed owner plus the statically known byte-only owners plus one spare for
the single split in flight. A row per quarantined token would consume rows nothing budgeted, and
the first quarantine to find none would hand a token back while Android still held its slot,
which is the fail-open the quarantine exists to prevent. Moving onto an existing row cannot be
refused for capacity, and the owner releasing that lease once releases every token with it.

**A settled transaction is not a delivered answer.** The resolver transaction's terminal ends two
things and not a third. The descriptor record ends, because the transaction is over and the descriptor is
closed; a logical token a closed transport handed over ends with it, because the platform's
slot really is over. The *answer* does not: the transport has still to receive it on its own
control channel, frame it, and hand each chunk to the client's stack. Releasing the whole
grant at the terminal gave capacity back for buffers that had not been created yet - every one
of them came into existence after the accounting said they were gone.

What may be delivered is decided *before* a delivery slot is taken for it. A delivery is a
grant its consumer gives back by naming it, so a value no acknowledgment could ever name -
this daemon's own wrapper failing, or a query too malformed for even a SERVFAIL - must not take
one: it would sit on the flow until the whole connection closed, which a client chooses the
length of. So the classification happens first, an answerable outcome is already that query's
own SERVFAIL by then, and what is terminal parks nothing and ends the stream instead.

So the query's grant is split at the terminal. What is left is a non-`Clone` **delivery** owner
holding the conservative peak of what follows - the result, the length-prefixed copy built
beside it, and the one chunk in flight - reserved as part of the original submission, so
nothing on that path is ever charged after it has been allocated. (The length prefix is in
that figure: `frame` allocates `answer.len() + 2`, and a bound of two maximum messages is two
bytes short of a maximum answer.) The delivery lives on the flow, and ends exactly once: when
the transport reports that the last chunk was acknowledged and its buffers are dropped, or
when the flow closes without that report because the consumer is gone.

**Classify, park, then hand the answer over - in that order, and unspellably so.** A
transaction's answer goes to the *owner*, not to the transport: the transport waits on the
depth-one control channel its flow was built with, and the owner sends on it only after the
delivery has been parked on that flow. The other order is a lost acknowledgment rather than a
leak. The answer used to travel straight from the resolver to a transport that was already
awaiting it, so a prompt transport could frame it, hand every chunk over, take the last
acknowledgment and report "delivered" *before* its own terminal had been read - and parking
happens at that terminal, so the report found nothing on the flow and did nothing, leaving the
grant there until the flow closed. The answer sits inside the settled delivery and parking is
the only thing that takes it out, so the wrong order is not something a caller can spell: there
is no call that yields an answer without having first put its delivery where an acknowledgment
will find it. And what is parked is decided *before* the park, because a delivery nobody could
ever name is a grant that ends only when the connection does.

**An acknowledgment names the answer, not just the flow.** A transport asks one question after
another on one connection, so every one of its acknowledgments names the same flow. The
delivery carries the submitted query's own identity - issued from a monotone counter its table
never reuses - and the owner releases only when the flow *and* that identity both match what is
parked. Without it, a late acknowledgment for a question already finished would release its
successor's grant while the bytes that grant covers were still being framed and handed to the
client's stack. A mismatched or duplicate acknowledgment is a no-op, and the flow's close is
what ends a delivery nobody acknowledged. A refusal carries no identity at all - it is a
different answer rather than an identity that merely looks invalid, because an identity an
acknowledgment path has to be *trusted* to recognise is one it can fail to.

**One piece exists, not one is queued.** The response is framed once, and each piece is copied
out of that framed buffer immediately before it is handed to the mailbox and is gone before the
next is copied - so the peak is the answer, the framed copy and one piece, which is exactly what
the delivery grant reserves. Counted at the allocation rather than at the consumer, because the
consumer sees one at a time either way: each of those three buffers is registered where it is
built and released where it is dropped, so an implementation holding more than three at once is
a failure rather than an argument. Building every piece first and handing them over afterwards
satisfies the mailbox's depth of one and holds a second whole copy of the response while it
does, in as many allocations as the read quantum divides into it. The depth bounds what is
*queued*; only the sequential handover bounds what exists.

**One flow's resources are taken and given back by one pair.** A registration acquires a
grant, a client-side socket with both its stack buffers, a worker identity and five bounded
channels, and any of the first three can refuse. Each failure unwinds exactly what preceded
it, and the reversal for failures that arrive *after* preparation - a worker table that
refuses, a fair queue that will not register - is that same reversal written once rather than
a second copy beside it. Two copies is how a socket outlives its lease. The socket leaves the
set with its buffers before the grant is released, so the aggregate never reads as free while
a buffer this daemon still owns is alive.

**Identities are checked, not wrapped.** A worker identity is what a terminal, a readiness
marker and a delivery acknowledgment are all matched against, so reusing one is not a counter
rolling over - it is every stale signal for a long-gone record landing on whatever holds that
number now, starting with identity zero, the first one the table ever issued. Allocation is
therefore checked and refuses at exhaustion, before anything is charged, opened or spawned, and
every admission path unwinds on that refusal. A `u64` cannot be exhausted by a real workload;
what it can be exhausted by is a bug, and a bug that fails closed is one that gets found.

**An answer is not a completion.** A resolver result arriving says the answer is ready; the
accounting may only move once the transaction is really over. There is no per-query worker
to hear that from: a submitted question is a row in a fixed, charged table that the ingress
owner polls, so what settles it is that poll taking the row out - one place, one order, and
no task whose lifetime a scheduler could interleave with the transport's. At that settlement
the descriptor record goes back, while the query and result buffers are split into a delivery
grant released only after the response has been built and the sources dropped - and the
platform's returned buffer is reconciled downward into the grant already reserved for it
rather than charged a second time.

**What a settlement does not generically do is return a logical token**, and the two protocols
differ here for a reason that is not cosmetic. A virtual-DNS *query over UDP* owns its token
outright - it is on that query's own grant - so its terminal is where the token ends or, if the
outcome was unobservable, is quarantined. A DNS-over-TCP token belongs to the *transport*: a
live one keeps it between questions, which is what lets the stream ask another, so settling a
question neither returns nor quarantines it. The only token a TCP settlement can end is one a
*closing* transport handed to that question's debt on its way out, because that grant is then
the last thing accounting for the slot.

The one exception is fail-closed rather than an ordering: a question whose *observation* was
lost, and whose token a closing transport had already handed to that question's debt, has
nowhere left to move the token to. Settling it would hand back capacity for a resolver slot
the platform still holds, so the buffers die and the grant carrying the token is kept for the
rest of the session instead - visible as an outstanding lease in the exit report.

### Timers

These timers govern only Rust's outer state:

| State | Minimum/default |
| --- | --- |
| UDP mapping and remote records | At least 2 minutes; recommended 5 minutes |
| UDP error record | Fixed absolute measured lifetime; never refreshed |
| Established TCP | At least 2 hours 4 minutes |
| Partial/opening/closing TCP | At least 4 minutes |
| Echo session | At least 60 seconds |
| IPv4/IPv6 incomplete reassembly | 60 seconds |

Outbound UDP activity refreshes only its own mapping and relevant remote.
Inbound packets, rejected packets, and ICMP errors do not. A `STOPPING` send may
use existing state but creates, tracks, or refreshes nothing.

#### A Flow Can Outlive Its Worker

A worker returns as soon as *its* ordered work is done: the upstream half-close is
written, the remote's end of stream has been handed over, and the client's stack
has taken it. The client's own teardown is not finished at that moment - the
socket is typically in `LAST-ACK`, `CLOSING` or `TIME-WAIT`, with a FIN to
retransmit and a final acknowledgement to wait for. So a clean terminal from a
flow nobody asked to stop **detaches** the flow rather than ending it:

- the worker's own state is already gone, because its task ran to completion and
  the upstream descriptor and everything the future owned went with it;
- the flow keeps its socket, both stack buffers, its conservative connection
  charge and its DNS state until smoltcp reaches `Closed`, its outer floor runs
  out, a config retires it, or the session ends;
- nothing stands behind it - no task of its own, and no *per-flow* timer task. Its
  teardown is still scheduled, by the combined smoltcp-and-outer deadline the owner
  already sleeps on, which is what lets a FIN be retransmitted. The ingress owner scans its own
  rows for a detached flow whose socket has closed, exactly as it scans for a
  settled resolver transaction, and that scan is what settles it.

Two endings are excluded, and both because there is no teardown to protect: a
*cancelled* worker also reports a clean terminal, and there the socket has already
been aborted by whoever cancelled it; and a socket that never got past its
handshake has no connection whose closing could be cut short. A worker that
*failed* is not a clean completion either - it resets its client and ends its flow
at once.

Config retirement and session shutdown recognise a detached flow and settle it
directly. Waiting for a second worker terminal would be waiting for one that can
never arrive.

#### Outer TCP Phases

The two TCP rows above are RFC 5382 section 5 REQ-5's floors, and which phase
gets which is REQ-5's own classification rather than a reading of the state
names. The daemon keys it on the actual post-action `smoltcp::socket::tcp::State`:

| `State` | Outer idle floor |
| --- | --- |
| `Listen`, `SynSent`, `SynReceived` | 240 s |
| `Established`, `FinWait1`, `FinWait2`, `CloseWait` | 7,440 s |
| `Closing`, `LastAck` | 240 s |
| `TimeWait` | none; smoltcp's own close timer owns it - `CLOSE_DELAY`, ten seconds in the pinned 0.13.1 |
| `Closed` | terminal |

`FinWait1`, `FinWait2` and `CloseWait` stay on the established floor because in
each of them one direction can still carry application data. Treating every
FIN-looking state as transitory would reset a client four minutes into a
half-close it is entitled to hold open.

**No post-RST retention is claimed.** RFC 5382 leaves what a NAT does after a
reset unspecified, and RFC 7857 later recommends holding a mapping for four
minutes after a matching one. This daemon does not: `Closed` is terminal, a reset
from either side ends the flow, and a tombstone would be new state created after
the terminating flow it describes is already gone.

These are idle floors, so matching activity rearms the whole of the current
phase's floor rather than topping it up. Two things rearm:

- a packet for the exact live tuple that was offered to smoltcp, rearmed from the
  phase the socket is in *after* the poll rather than before it;
- a worker event on the exact current `(SocketHandle, worker)` that really
  delivered payload into the fair owner or a real ordered end of stream. A DNS
  answer chunk is payload like any other.

**"Offered to smoltcp" is deliberately coarser than "accepted by smoltcp", and the
difference is real.** The ingress parse reads the four-tuple, the hop limit and the
SYN bit and nothing else - no checksum, no window, no state - so a segment for a
live tuple that smoltcp then discards, for a bad checksum or for being outside the
window, still rearms that flow. Answering "did the stack take this?" would mean a
second TCP implementation beside the one the packet was just handed to, which is a
worse trade than a client holding its own idle connection open with segments it is
already free to send. What is refused *before* the stack sees it does not rearm:
a packet the ingress parse rejects, one whose destination has no flow and is not
opening one, and one the device refused because the previous packet had not been
consumed.

Nothing else rearms either. Not stack output, retransmissions or delayed
acknowledgements; not a reset or acknowledgement this daemon originated; not a
config being applied; not a stale identity or a flow already retiring. And not
while `admitting` is false: a `STOPPING` session drains what it already owns -
queued payload still reaches the client - and tracks nothing new. Its deadlines
keep running, because stopping is not pausing.

Expiry is the ingress task's, on the same wake as smoltcp's own timers: the owner
polls the stack first and then applies whatever was due at the instant it
captured *before* that poll. That defers by one loop anything that came due while
the stack was running - the poll advances smoltcp on its own reading of the clock,
and judging a floor against a later reading would expire a flow against a moment
the stack had not been asked about. Deferring is the conservative direction, since
a floor is a minimum, and the next wake is immediate because that flow's deadline
is by then in the past.
A due flow is retired in the engine's ordinary order - discard its queued payload
and end of stream, cancel that flow's own token, drop the upstream write half,
abort the client socket, and poll once so the reset is a packet that was really
built under the stamp current at that moment. A reset is counted only where the
stack has somewhere to send one: a socket with no remote endpoint - one still
listening, or one already closed - is aborted silently, so an expiry can end a
flow without any client-visible packet at all. Whether it reaches the wire is the
writer's ordinary business rather than a stronger promise: a config that changes
the stamp before the writer dequeues it purges this packet exactly as it purges
every other one of the retired stamp. Only the flow's own token is cancelled; the engine-wide sweep token means
"the network these are bound to is being left" and selects `SO_LINGER(0)`, which
an idle flow's upstream has no reason to ask for. Nothing is removed or refunded
there: the record, the descriptor and the charge go when the flow is finally
settled, like every other ending. Expiring a DNS-over-TCP transport ends the transport
only - its resolver transaction is not cancelled, awaited or refunded, and a
question still in flight keeps its row and its logical token until the platform
is done with it.

## Lifecycle And Cleanup

Normal stop from any committed state is ordered:

1. Enter `STOPPING`. Close new flow, mapping, DNS, Echo, fragment, and memory
   admission. Existing committed work may finish only within already owned
   capacity and may not grow collections.
2. Call `setPreferTestNetworks(false)` through the pinned connector and await its
   result code, failure, or timeout. If its epoch is dead, mark the clear pending
   for the cleanup-only epoch in step 5. A non-`TETHER_ERROR_NO_ERROR` or unknown
   result is reported, but cleanup continues.
3. Send Rust shutdown, close the control socket, and apply the 10-second/5-second
   process-exit escalation.
4. Call `NetworkAgent.unregister()` exactly once. Await
   `onNetworkDestroyed`/request `onLost` within the deadline.
5. Release the exact retained request through the pinned manager. If its Binder
   epoch is dead, admission remains closed while a cleanup-only epoch is obtained
   with the same effective UID, performing only any pending preference clear
   through a fresh connector and the idempotent request release through a fresh
   private manager. A missing request is successful cleanup.
6. Close every remaining TUN PFD and retire the generation. Drop the private
   manager, agent context, callback, and agent references only after their
   corresponding cleanup is confirmed; otherwise retain the minimum cleanup
   state without permitting another session.

Root mode is not part of that order, and there is nothing to coordinate. The two modes are independent:
root runs its existing per-interface routing, this mode publishes one global upstream and lets Android's own
tethering select it or not, and neither is started, stopped, delayed, refused or rebuilt by the other.
Starting this mode while root routing is live is allowed; starting root routing while this mode is running
changes nothing about it. When both are up, root's own routing takes precedence over whatever upstream
Android picked - that is the ordinary root design doing what it always did, not an arbitration this feature
implements. `RoutingManager.start`, `stop` and `clean` are `master`'s, and nothing in them consults this
mode.

What this mode does serialize is only its own lifecycle, in
[`ShizukuLifecycle`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/ShizukuLifecycle.kt): one command
at a time, one session at a time. A duplicate press shares the start already in flight rather than beginning
a second; a stop issued during publication queues behind it and withdraws the session the moment it lands;
a stop that arrives while the start is still in its *interactive* half - Shizuku authorization, which can
sit on a permission dialog for as long as the user takes - supersedes it instead, because nothing has been
created yet and there is nothing to withdraw. A supersession is still reported as a stop in progress until
the superseded start has unwound, because until then that start still owns the flight a duplicate press
would share: reporting off there let the next start join a flight that was already doomed. From the moment
publication begins the start either completes or rolls back; it is not cancellable, because a half-published
session is the residue this exists to prevent.

Rollback has exactly one owner, and it is the lane. A publication that fails throws with its ledger intact -
every resource it created is recorded in the same ledger a retirement withdraws - and the lane runs the one
retirement over it. The publication does not also unwind itself: two owners meant a failed rollback was
retried immediately by the start that caused it, rather than left for the next explicit stop as the rule
below says.

Failure is fail-closed and is not claimed to be atomic. A publication that throws is retired before the
failure is reported, because it can leave a child or an agent behind. If that retirement also fails - for
any reason, a deadline and a replaced Shizuku epoch included, both of which arrive shaped as cancellations
from inside a `NonCancellable` withdrawal - the mode keeps reading as on, the retirement error is attached
to the publication error as a suppressed cause, and the session is kept so the next stop is a retry rather
than a fresh start. A stop whose retirement throws behaves the same way. None of those paths touches root
mode: what a failed retirement blocks is a *successor Shizuku session*, and nothing else.

The boundary for "this session is gone" is exactly where the local resources end: the child has exited and
been joined, the agent's withdrawal is proven, the session's observers are joined, the descriptor is closed,
and the current cleanup attempt has returned. The agent barrier is `onNetworkUnwanted`, which
ConnectivityService delivers from `NetworkAgentInfo.disconnect()` whether or not a native network was ever
created; `onNetworkDestroyed` is additionally required when `onNetworkCreated` arrived and the request's
`onLost` when it reached `onAvailable`. The absence of a callback is never itself proof, which is why an
agent or exact request whose outcome is *unknown* keeps the mode reading as on: nothing local can be fenced
from a silence, and the only remaining release is process death.

Across app processes there is no lock, and none is needed: nothing here is shared with another process's
root routing. Within this process the session ledger is the only thing a successor waits on.

If request release remains unavailable or unknown, all local resources are
still withdrawn, but another Shizuku session in this app process is forbidden.
Retain enough state to retry cleanup and require an app-process restart if it
cannot be confirmed. Callback/agent Binder death on process exit is the final
release path. Android tethering may continue on an ordinary upstream after
Shizuku cleanup.

#### Platform Residue This Mode Cannot Clean

One residue outlives every cleanup path, and it is the platform's rather than
this app's: each session leaves a pair of rules in netd's `tetherctrl_counters`
chain naming the TUN, and they are never removed when the interface goes away.
Observed directly - forty rules naming twenty dead `testtunN` interfaces after
a day of testing, in that chain only, with nothing of this app's own left
anywhere in either family's tables.

This is specific to the design rather than a generic tethering quirk. Ordinary
tethering reuses `wlan2` or `rndis0`, so netd reuses the same counter rules
across restarts; `createTunInterface` assigns a fresh index per session, so
every session contributes a pair that can never be matched again. Growth is
therefore driven by session churn and bounded only by reboot, and rule
evaluation is linear, so a long-lived device that toggles the hotspot often
pays a slowly growing per-packet cost on tethered traffic.

Nothing at the app UID can remove them, and this mode must not try: they live in
a chain netd owns, alongside rules for live interfaces, and deleting platform
state that merely shares an interface family is exactly what the reversibility
rules forbid. Root mode could, but this is the mode that has no root. So it is
documented, left alone, and belongs in step 11's device qualification as a
measurement rather than in a cleanup path.

### Upstream Fallback

Withdrawing the agent does not stop tethering, and clearing the preference does
not either. When no TEST network is present, tethering's upstream selection falls
straight through to the default or DUN network
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/UpstreamNetworkMonitor.java#328)),
and losing a network is itself a reselection trigger
([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/Tethering.java#2297)).
So every path that removes the agent - orderly stop, terminal session failure,
app-process death, force-stop - hands the hotspot back to the ordinary upstream
while it stays up. Clients keep working, which is exactly why this is dangerous:
nothing on the client side changes when the protection stops. Shizuku death is
not such a path once the session has committed: the agent is app-hosted, so it
and the dataplane survive, as [Process Death](#process-death) describes. Before
commit it is an ordinary startup failure, and the rollback removes the agent like
any other.

This is not fixed here, by decision rather than by omission. Android apps are
meant to be killable, and the paths that matter most run no code at all; a
blocker that survived app death would need a persistent privileged process this
architecture does not have. Root mode does not share the limitation, because a
privileged daemon is still running when the app goes: losing the control socket
is EOF on its control loop, which runs `state.stop(false)` and tears every
session's routing down. `Clean` is the recovery path for residue a teardown
could not remove, not the thing that ordinarily ends root's routing.

What this mode offers is best effort while a session holds: traffic does not
egress outside the selected network while the daemon is running it. Nothing is
closed on the way out, here or anywhere else in this document. Explain the
behavior to users in `README.md`.

### Process Death

- App-process death closes its TUN PFD, agent Binder, request callback Binder,
  and Rust control socket. ConnectivityService removes the agent/request. Rust
  must react to EOF, close its duplicate TUN FD, and exit; a surviving child or
  TUN is a production blocker.
- Shizuku death after commit leaves the app-hosted request/agent alive but makes
  old-epoch wrapped operations unavailable. Agent withdrawal and local FD/child
  cleanup still work; request and preference cleanup use a cleanup-only
  reauthorization or wait for app-process death/manual recovery.
- Force-stop and uninstall kill the app process and app-UID child and release
  their Binder/PFD-owned state.
- None of these abnormal paths can clear the global preference.
- The ones that end the app process - app-process death, force-stop, uninstall -
  also trigger the upstream fallback above, and leave nothing running to report
  it. Tethering stays up on an ordinary upstream, and Android's own tethering
  notification still shows the hotspot as running. Shizuku death after commit is
  the exception: the session keeps running on its own agent and dataplane, so the
  fallback does not occur and the app is still there to report the degraded
  state. Before commit it is an ordinary startup failure and rolls back with the
  rest.

A newly started app process has no old in-memory generation. Before publication,
it still rejects any surviving TestNetwork collision and never automatically
replays the previous session.

### Global Preference Recovery

The global preference has no owner token. Orderly rollback and stop always attempt to clear it. App death,
force-stop, or uninstall can strand `true`.

**There is no separate recovery action, and no Settings surface for one.** An earlier draft specified an
explicit Settings row that wrapped an `ITetheringConnector` and cleared the flag on demand; it was removed
along with the rest of the scope that touched screens this feature does not own. What clears the flag is the
mode's own row, and there are two cases. Inside one app process, a session left `RESIDUAL` by a withdrawal
that could not finish is retried by the next start's own preparation - a fresh epoch is exactly what the
outstanding clear was missing - so the clear is settled before anything new is published. Across a crash the
ledger is gone with the process, so nothing retries it: what clears the flag then is the next session's own
stop, or a reboot.

The accepted limitation, stated rather than engineered around: from a crash until the mode is next started
*and* stopped, the flag stays set. That is not the same as the hotspot being stranded - with no TEST network
present, tethering's upstream selection falls straight through to the ordinary default, which is exactly the
fallback described under [Upstream Fallback](#upstream-fallback). What the stale flag actually costs is that
the *next* TEST network to appear is preferred, whether it is this app's own later session or another app's.
Another TestNetwork controller is unsupported, so value restoration and coexistence are not attempted.

## Failure Semantics

A session ends only when it cannot run, never as containment. Ending it removes
the agent, which hands the hotspot to an ordinary upstream, so a defensive
teardown makes the outcome worse rather than safer. Everything that is merely
unknown or degraded is a structured report and continues.

Rare failures end the session explicitly instead of being recovered in place. The
escape hatch is worth more than an automatic recovery path, because a path that
only runs in a state nobody can reproduce is untested code that must nevertheless
be correct about ownership, epochs, and process death. Prefer an explicit
non-silent failure and a working button. This is why the wedged-daemon case ends
the session rather than relaunching the child.

For this mode the hatch is stop and reapply. Tethering connector death is the one failure it does not cover: reapply
recollects against a connector the platform caches permanently, so that failure
reports a required app restart instead, and a generic failure UI must not offer
reapply for it. Root mode's `Clean` is not the hatch either, and it is not a
cross-mode action: it undoes root routing and firewall mutations this mode never
makes and cannot make under the app UID, it owns neither this session's request
nor the global preference, and it neither stops nor disturbs a running session. Reapply covers everything an ordered
stop already released. The preference is the one piece of system state that outlives
the session, and nothing beyond a session's own stop clears it - see
[Global Preference Recovery](#global-preference-recovery).

Startup failures are terminal because there is nothing to preserve yet:

- missing authorization, Binder, permissions, hidden API, exact runtime
  overload, unresolvable effective-UID package, or privileged-manager field or
  `sInstance` readback failure;
- native launch/authentication/FD-transfer failure;
- TUN, request, agent, preference, publication, or security-readback failure;
- unexpected agent/request loss before or during commit;
- immutable MTU/config mismatch;
- selected-network recursion or a different TestNetwork collision;
- startup/callback deadline expiry.

A committed session ends only when its own machinery is gone:

- a control connection that cannot carry or confirm an update, since the app then
  cannot tell what the child is bound to and cannot make it stop;
- an unusable TUN, or a packet writer whose invariants persistently fail;
- an unusable resource-admission owner, since the shared principals are all that
  bounds what forged input can make the daemon allocate;
- unexpected agent or request loss after commit, which has already removed the
  network this session exists to run.

Rollback removes app-owned state but never stops Android tethering, and cannot
remove the `TestNetworkService` singleton the first acquisition creates inside the
system server.

Local/optional failures include:

- losing Echo or remote-ICMP translation for one family while TCP/UDP continue;
- dropping malformed or unsupported packets;
- rejecting one flow/datagram/query on bind, option, route, descriptor, or memory
  failure with no default fallback;
- applying backpressure to an existing TCP flow instead of allocating;
- consuming an unmappable UDP error without translation;
- retiring one mapping when its selected Network, source address, or socket is
  lost.

Unexpected background failures must become structured daemon reports with
operation, family, principal, safe tuple context, selected Network, errno, and
Rust source location. Hostile packet input must not create a report flood.
`platform_dns`, `platform_ipv4`, and `platform_ipv6` must never be labeled as
physical clients, in reports or in the UI, because none of them is one.

## Implementation And Validation

Implement in this order:

The order is driven by risk, not by build convenience. Steps 1-3 cost days rather
than weeks and need no dataplane at all. Steps 1 and 2 are done; step 2 failed and
redefined the mode instead of ending it, so step 3 onward proceeds under
[Security Posture](#security-posture) rather than under an isolation premise.

1. **Done.** Prove the in-process Shizuku Binder path, constructor-less manager
   construction with unchanged `sInstance` on 13-17, effective-UID attribution,
   private agent context, restricted agent, exact request, readback, and
   complete cleanup.
2. **Done, failed.** Characterize the boundary on device: run the separately signed
   attacker APK against the restricted agent, and the owner app's own UID, over
   every denial listed under
   [Restricted TestNetwork Publication](#restricted-testnetwork-publication) plus
   direct interface and source selection. Handle selection is denied; interface
   selection is not. Verdict and reproduction are below.
3. **Done.** Prove the platform path end to end with no dataplane behind it: check
   that the device uses automatic upstream selection at all, set the preference,
   confirm tethering selects the restricted TestNetwork, confirm the downstream
   receives a delegated `/64` from the documentation prefix, and confirm forwarding
   is installed on the chosen IPv4 prefix. Delegation on a restricted agent was the
   specific unknown; the prototypes only show it on an unrestricted one.
4. **Done.** Prove app-UID native launch, peer credentials, nonce authentication,
   and one-FD `SCM_RIGHTS`.
5. **Done.** Native bootstrap, the immutable MTU check, the IPv4 gateway-address check, the
   descriptor-owning writer task and the packetization core, all on device. The rest of
   address verification is impossible at the app UID rather than outstanding: netlink
   binding and `/proc/net` are both denied, measured on device, so there is no IPv6
   equivalent - and the virtual-DNS addresses were never verifiable in principle, because
   they are deliberately not assigned to the interface. See
   [Step 5 Status](#step-5-status).
6. **Done.** Prove selected-network TCP, unconnected multi-destination UDP, ping
   sockets, hop metadata, IPv4 DF modes, and error queues without default fallback.
7. **Done.** Add UDP/DNS classification, resource owners, return packetization, and
   timers. See [Step 7 Status](#step-7-status).
8. **Done**, apart from two edge cases that need a real client. Add TCP. A real connection
   completes through the terminating stack on device, per-flow output has landed - each flow
   has its own depth-one mailbox at the read quantum rather than one global pending chunk - TCP
   port 53 routes into the resolver handoff through the terminating engine, and the
   DNS-over-TCP generation semantics around it have landed too. The client-closes-first
   half-close is device-proven. What is left is a client that **resets** mid-stream and a
   **simultaneous** close: neither is arrangeable with the tools on the device, so both belong
   to the real-client testing deferred to step 11. See [Step 8 Status](#step-8-status).
9. **Done**, apart from one deliberate exclusion. Echo, the three originated ICMP errors,
   dual-family ingress reassembly, the send history, repeating a remote's Packet Too Big,
   Time Exceeded and Destination Unreachable for both UDP and Echo, and the bounded
   extension-header walk. Parameter Problem is excluded on purpose: its pointer names a byte
   of a header the daemon rewrote, so it needs a pointer mapping rather than correlation. The
   walk is unit tested but not device-proven, because no tool on the device can inject an
   extension header. See [Step 9 Status](#step-9-status).
10. **Done**, with one item dropped rather than built. The daemon-side handover and
    selected-network observation landed; tethering states landed with step 3; and the mode's
    own start/stop control is in: `ShizukuTetheringService` publishes the session's state and
    the tethering screen renders one global row - a switch that starts and stops the mode -
    with the failed-withdrawal case offered as a retry rather than a dead toggle. The manual
    preference-recovery UI was **removed from scope**: it was a Settings surface this feature
    does not own, and the residue it addressed is documented under
    [Global Preference Recovery](#global-preference-recovery) instead. See
    [Step 10 Status](#step-10-status).
11. Complete compatibility inventory, resource measurements, device
    qualification, and documentation. The inventory and the documentation are done; the
    measurements and the qualification need a device.

Each slice must include complete rollback and stop behavior.

### Implementation Status

The mode lives entirely in
[`mobile/src/main/java/be/mygod/vpnhotspot/shizuku/`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/),
including its own command lane,
[`ShizukuLifecycle`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/ShizukuLifecycle.kt), which
serializes this mode's starts and stops and nothing else. It is reached from one row on the tethering screen
and
[`ShizukuTetheringService`](../../mobile/src/main/java/be/mygod/vpnhotspot/ShizukuTetheringService.kt); the
debug-only broadcast receiver that drove Step 1 before there was a UI has been removed, so nothing in
`mobile/src/debug` drives it any more.

Step 1's mechanism was proven on Android 17 with root-backed Shizuku: the in-process Binder
path, constructor-less manager construction with an identity-unchanged `sInstance` and an ordinary
context that still returns the ordinary manager, TUN creation, the exact foreground request, the
restricted agent, the publication barrier, and complete cleanup with no leftover network, request or
interface across repeated sessions.

That device's Shizuku runs as **root**, so it exercises the uid-0 attribution branch, where the app
package stays valid. `ConnectivityService` logged the request as root-owned with
`RequestorPkg: be.mygod.vpnhotspot`, and the published capabilities were
`TEST … NOT_METERED NOT_VPN NOT_SUSPENDED NOT_VCN_MANAGED NOT_BANDWIDTH_CONSTRAINED` with no
`NOT_RESTRICTED` and no `INTERNET`. Still unproven: the `com.android.shell` attribution branch, which
needs Shizuku restarted from wireless debugging; Android 13-16; OEM and Mainline variants; and the
per-operation transaction count, which needs the wrapper counted on a device by hand - see
[Required Security And Lifecycle Tests](#required-security-and-lifecycle-tests).

Where the implementation departs from the prose above:

- **The specifier is lifted, not constructed.** `NetworkRequest.getNetworkSpecifier()` is `public-api`,
  so the exact request is built first and the agent's capabilities reuse that same
  `TestNetworkSpecifier` instance. This drops the blocked `TestNetworkSpecifier` constructor and makes
  agent/request specifier identity structural instead of asserted.
- **`setAllowedUids(emptySet())` is not called.** A fresh `NetworkCapabilities.Builder` already carries
  an empty allowed-UID set on Android 13-17, so the blocked setter would be a no-op, and this document
  already notes the value cannot be read back reliably. What produces the restricted netd network is
  the absence of `NOT_RESTRICTED`.
- **`TRUSTED` is removed as well as `NOT_RESTRICTED`.** A fresh builder starts from
  `NOT_RESTRICTED|TRUSTED|NOT_VPN`, and AOSP's own test networks carry neither of the first two.
- **The publication barrier requires a subset, not equality.** ConnectivityService adds `VALIDATED`,
  `NOT_ROAMING` and `FOREGROUND` to the capabilities and normalizes routes, so the barrier requires the
  submitted addresses, routes and DNS servers back, the exact transport, specifier, interface and MTU,
  and the absence of `NOT_RESTRICTED`/`INTERNET`. Requiring exact equality would fail on a correct
  device.
- **The publication callbacks are three separate barrier inputs.** Repeated publication observed
  `onAvailable` arriving before the initial capabilities and link-properties callbacks, so the owner
  awaits all three rather than treating availability as proof the readback had arrived.
- **No run-to-completion records yet.** The three uncancellable calls run on the session's own lane and
  nothing proceeds without their results, so no orphan can exist. Those records become necessary only
  when something else must proceed on a deadline.
- **`NET_CAPABILITY_NOT_BANDWIDTH_CONSTRAINED` is probed.** It is added when the installed module
  accepts it and skipped on `IllegalArgumentException`, since the constant does not exist on Android 13.

### Step 2 Verdict: Failed

Reproduced on Android 17 with root-backed Shizuku. Preflight confirmed distinct owner and attacker
app UIDs, different signing certificates, and that neither UID held the restricted-networks,
network-settings, or test-network-management permissions.

The framework and netd checks all did work. Restricted active TEST requests threw `SecurityException`;
unrestricted TEST requests did not match the restricted agent; Java and NDK full-`Network` socket
selection failed with `EPERM`/`EACCES`; process binding failed; `SO_MARK` failed with `EPERM`. A
shell-UID `app_process` positive control selected the handle successfully and delivered both families.

None of that protects the Linux interface. Continuous capture on a duplicated TUN descriptor observed
each UID's unique IPv4 nonce arrive from two paths, for the owner UID and the separately signed
attacker alike:

- `SO_BINDTODEVICE` on the `testtun` interface name;
- `sendmsg` with `IP_PKTINFO` carrying the `testtun` interface index and a source address in its
  prefix.

An otherwise identical unrestricted agent - same owner, request, publication barrier and cleanup path,
changing only `NOT_RESTRICTED` - calibrated every route-capable path plus those two, which is what
rules out a harness artifact. This is a packet oracle, not an inference from a socket return value.
Cleanup removed both test networks and their TUNs after the failing runs.

The harness itself was not kept: it was a separately signed attacker APK, a shared Java/NDK probe
library, and an instrumented test that published a session and read the TUN. None of it lived in this
repository and none of it can, because this repository deliberately has no instrumented test component -
no `src/androidTest`, no runner, no `androidTestImplementation` dependencies, and `enableAndroidTest =
false` on every variant. Re-deriving it is roughly a day. Anyone repeating this should reproduce both paths above before trusting a passing result,
because everything else about the boundary passes.

### Step 3 Verdict: Passed

Same device as Step 2, with Wi-Fi tethering on `wlan2` and the ordinary upstream on
`wlan0`. Every claim below is from `dumpsys tethering`, `ip`, and `iptables` while a
session held the network:

- `setPreferTestNetworks(true)` returned `TETHER_ERROR_NO_ERROR` through the
  listener on the wrapped connector.
- **A restricted TestNetwork is selected as a tethering upstream** - but not
  reliably. It was observed on two separate TestNetwork interfaces, with the
  session reaching `ACTIVE` on the same `Network` both times. Step 7 then found the
  same sequence choosing the ordinary upstream repeatedly, which is a race rather
  than an ordering rule; see [Selection Is Racy](#selection-is-racy).
- **The full state machine was exercised in one run:** `RESTART_REQUIRED` at commit
  (tethering already on `wlan0`), then `ARMED` when the downstream went away and
  tethering dropped its upstream, then `VERIFYING` when an upstream returned that could
  not yet be classified, then `ACTIVE`. That run predates the global path, which reaches
  the same four states from the upstream observation alone.
- **IPv6 delegation works on a restricted agent**, which was the specific unknown:
  the downstream received `2001:db8:1::26/64` from the TUN's documentation prefix,
  and the offload table showed the prefix delegated toward the TestNetwork TUN with `pmtu 1500`, so
  MTU 1500 propagated as designed.
- **IPv4 forwarding is installed on the `/30`:** `tetherctrl_FORWARD` gained
  a `wlan2`-to-TestNetwork-TUN rule with the matching conntrack rules, and
  `tetherctrl_nat_POSTROUTING` gained MASQUERADE toward that TUN, which is why the
  interface address has to be present and usable.
- **Upstream fallback is real and immediate.** Stopping the session returned the
  upstream to `wlan0` while the hotspot stayed up, with no client-visible change -
  the hazard [Upstream Fallback](#upstream-fallback) describes.
- Cleanup left no TUN, no live TEST network, and no live request, and the preference
  clear returned no error.

Nothing here involved a dataplane: packets reaching the TUN were dropped, because
nothing was reading it.

### Step 4 Verdict: Passed

Same device. The binary launched from the app process runs as the app UID
(`u0_a463`), parented to the app rather than to a root shell, holding `/dev/tun` on
one descriptor. The three-frame handshake in
[`bootstrap.rs`](../../mobile/src/main/rust/vpnhotspotd/src/bootstrap.rs) completed,
the daemon confirmed the descriptor is a nonblocking TUN naming the expected
interface, and stop produced `exited with 0` with no orphan process, no TUN, and no
live TEST network. Escalation past EOF was never needed, so SIGTERM and SIGKILL are
still unexercised.

Four things went wrong first, and all four are the kind of mistake that only a
device finds. They are recorded because each is a trap for the next slice:

- **Ancillary descriptors attach per `write`, not per frame.** A `DataOutputStream`
  length prefix is four single-byte writes, so the TUN arrived five times over. The
  frame is now built in one buffer and written once.
- **The accepted `LocalSocket` is nonblocking**, so plain stream reads return
  `EAGAIN`. Reads go through the nonblocking channel; only the descriptor-carrying
  write uses the socket directly, because that ancillary state belongs to
  `LocalSocketImpl`'s output stream and a channel writing the raw descriptor by
  another route drops it silently.
- **`java.lang.Process.pid()` is not on this project's compile classpath**, so the
  fence signals the peer-credentials pid instead. That is the better identity
  anyway: it names the process actually holding the connection.
- **`ioctl`'s request argument is `c_int` on bionic and `c_ulong` on glibc**, so a
  host `cargo check` compiles what the Android target rejects. The Gradle native
  task is the real check for anything touching `libc`.

The plan launches the daemon before the TUN exists and transfers the descriptor
afterwards; this slice couples the two, because splitting them would mean holding a
half-finished handshake for no gain until there is a dataplane to configure.

The handshake carries **no protocol version**, though earlier drafts of this document
asked for one. The daemon is exec'd in place from the same APK as the dex that
launched it, so the two cannot disagree: an app upgrade kills the app's processes and
removes the old APK path, a leftover daemon from a previous version connects to a
socket name that no longer exists, and a tampered APK controls both sides anyway. The
nonce answers the only question that can vary, which is whether the peer is the
process this launch created. Add a version only if the daemon ever ships or updates
independently of the dex.

### Control Surface Decision

The Shizuku daemon gets **its own control surface**, not `ClientEnvelope` /
`SessionConfig` / `DaemonController`. This supersedes the adoption note below asking
for the downstream epoch and admission state to be added to `SessionConfig`.

The existing session vocabulary is routing-shaped - `downstream`, `masquerade`,
`ipv6_block`, `ipv6_nat`, per-MAC `clients`, `primary_routes` - and every mutation
behind it needs root: netlink writes, `iptables-restore`, `ndc`, `/proc/sys`, fwmark,
TPROXY, NFQUEUE. A Shizuku session would populate almost none of those fields and
could not act on them if it did. Sharing the message would mean one type whose
meaning depends on which UID is reading it, which is exactly the kind of ambiguity
the epoch and admission fields must not inherit.

What the two paths share stays deliberately small: the binary and its ABI check, the length-prefixed frame
format, the report *builders*, and `daemon.proto` as the single file describing both conversations. That is
the whole list, and it was cut down rather than grown. The report frame is not shared - `ShizukuDaemonFrame`
carries an acknowledgement or a report, because every other frame in `DaemonEnvelope` belongs to a call/reply
vocabulary the app-UID path does not have. Neither is the *delivery*: root keeps its `NonfatalCoalescer` keyed
on `(context, kind, errno, file, line)` behind a process-global channel, and the app session gets its own
site-keyed `SiteCoalescer` behind a per-session reporter, because their flush, ownership and failure semantics
differ. Root's DNS, nonfatal, session, routing and NAT66 code is untouched - IPsec is the authorized
exception, where the probes are now process-owned and generation-stamped; the downstream epoch, the
admission state, and the applied-generation acknowledgement belong to the new Shizuku messages.

### Step 5 Status

Done:

- **Immutable MTU verification.** The bootstrap reads the interface's MTU with
  `SIOCGIFMTU` and refuses a mismatch, rather than trusting the value the app
  declared. Verified on device.
- **The packetization core**, in
  [`shared/packet_writer.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/packet_writer.rs):
  final size validation that rejects a declared-versus-actual length disagreement,
  IPv6 source fragmentation, and the size policy that decides which datagrams may clear DF.
  Unit tested, including a reassembly round trip and the 8-byte alignment rule.

  Two decisions worth knowing. Fragmentation rejects a datagram whose first extension
  header belongs to the unfragmentable part - Hop-by-Hop, Routing, Destination Options
  - rather than splitting it wrongly; nothing in this design emits those, so the check
  is an assertion about the daemon's own output. And Identification is a per-tuple
  sequence rather than a global counter or a random value, because a receiver
  reassembles on source, destination, protocol and Identification, so only same-tuple
  reuse can mis-splice; the map is bounded because any local app can influence the
  tuple set.

  The allocator itself has since moved next to it, into
  [`shared/ipv4_identification.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/ipv4_identification.rs),
  along with the temporal state that makes the sequence exact rather than merely per-tuple.
  The line above about eviction restarting a sequence described the gap that state closed;
  see [The Nonreuse Window](#the-nonreuse-window) for what replaced it.

Since done: the descriptor-owning writer task, with its bounded queue, its `EAGAIN` wait
and both backpressure sources, which arrived with step 7 and is device-proven.

**Address verification is mostly impossible at the app UID, and that is now measured rather
than assumed.** The original entry said it "needs an address enumeration it does not
otherwise have"; the answer is that no such enumeration exists to be had. Probed on device
by running the debug build's own UID through `run-as`:

- binding a `NETLINK_ROUTE` socket - which is what `getifaddrs` and `ip addr` do - fails
  with `Cannot bind netlink socket: Permission denied`. From the shell UID the identical
  command succeeds, so this is the app-UID sandbox rather than a missing tool;
- `/proc/net/if_inet6`, the only other way to read IPv6 addresses, is `Permission denied`.
  So is `/proc/net/route`;
- `/proc/self/fd` does work, which is the one such path this design already depends on -
  the descriptor budget counts it.

So the only way an app-UID process can learn an interface's addresses is an ioctl on a
descriptor it already holds. That leaves exactly one verifiable field: `SIOCGIFADDR` on the
transferred TUN would confirm the **IPv4** gateway address, by the same mechanism as the MTU
check beside it. There is no IPv6 equivalent ioctl, so the IPv6 gateway address cannot be
verified by any means available here.

The virtual-DNS addresses were never verifiable even in principle, and noting that corrects
the original framing rather than excusing it: they are deliberately *not* assigned to the
interface - they exist to be intercepted - so there is nothing on the interface to compare
them against. What guards them is that they are compared before attribution, reassembly or
transport dispatch, which step 7 does.

What remains implementable is therefore one ioctl covering one field, and it is now done:
`verify_tun` reads the address where the descriptor is, and the session refuses any config
whose declared IPv4 gateway disagrees with it. Verified on device - a real session reaches
`ACTIVE` and every relay path still works, so the ioctl returns the declared `192.0.2.1`
rather than rejecting a legitimate config.

Worth doing for the same reason the MTU check is: an ICMP error sourced from an address the
TUN does not hold is one a client would either ignore or believe, and neither is what the
daemon meant to say. Not worth pretending the rest is pending.

### Step 6 Verdict: Passed

Every primitive works from the app UID, first run, both families. The probe lives in
[`egress.rs`](../../mobile/src/main/rust/vpnhotspotd/src/egress.rs) and runs through
the same functions the dataplane will use rather than through a separate harness, so
a passing probe is evidence about the production path. It is debug-only: the app
sends probe targets only in a debug build, and an empty list means nothing runs.

Against the selected network's own resolvers, which is the smallest legitimate target
available:

- **`android_setsocknetwork` from the app UID works**, which was the one real unknown.
  UDP and TCP sockets bound to the selected network in both families.
- **Unconnected multi-destination UDP works**: two destinations reached through one
  socket per family, which is what makes the outer mapping endpoint-independent.
- **Hop metadata arrives**: `hop_limit 56` and `54` on IPv4, `56` and `54` on IPv6 -
  real wire values, not defaults, so relayed traffic can preserve them instead of
  substituting `local_origin_hop_limit`.
- **The receiving interface index arrives** (47 on every reply), which is the cheap
  half of the late-reply defence.
- **Both IPv4 DF modes are settable**: `IP_PMTUDISC_DO` and `IP_PMTUDISC_OMIT`, the
  latter spelled out because `libc` does not export it for Android.
- **Ping sockets open in both families**, so unprivileged Echo is available.
- **Error queues are readable**: `MSG_ERRQUEUE` returns `EAGAIN` when empty, which is
  the correct empty-queue answer rather than a failure.

There is no fallback path in the code to test: every socket binds explicitly and a
bind failure returns to the caller.

One trap worth carrying forward, met twice in two slices: `libc` types differ between
bionic and glibc, and the repo's tooling checks the host while the product ships the
target. `ioctl`'s request argument is `c_int` versus `c_ulong`, and
`in6_pktinfo.ipi6_ifindex` is `i32` versus `u32` - the latter making a `try_from`
mandatory on one and a `-D warnings` lint error on the other. Both are resolved by
normalizing rather than by `cfg`.

### Step 7 Status

The control surface is implemented end to end and verified on device. The app computes
a config and sends it; the daemon retires, applies, and acknowledges both axes; the app
only believes admission once the acknowledgement says so.

- **`app_session.rs`** replaces the bootstrap's EOF wait. It rejects a config that
  moves either axis backwards, retires before acknowledging - the ordering that lets
  the app reopen admission safely - and sets admission last and only from the config.
  Retirement is now a request that is *answered* rather than a notification: the config
  goes to the ingress task, which owns every piece of epoch-keyed state, and the
  acknowledgement waits on that task confirming the previous epoch is gone. Nothing else
  makes that ordering literally true, since the session loop owns no dataplane state and
  so cannot know when the state is retired.
- **`AppUidDaemon.apply`** coalesces through a single pending slot. The slot is
  necessary even though every caller runs on one dispatcher, because the call suspends
  awaiting the acknowledgement and another observation can reach the lane in that
  window - a single-threaded dispatcher orders dispatches, not run-to-completion
  sections. A superseded caller returns normally rather than waiting for an
  acknowledgement that will never name its config.
- **The MTU floor was measured when this step ran**: `min(MTU)` over the downstreams
  tethering reported, via `NetworkInterface.getByName(name).mtu`. That is no longer what
  ships. The mode is global rather than per-downstream, so the floor is now the fixed
  `test_network_mtu` contract - see
  [MTU, Output, And Fragments](#mtu-output-and-fragments) - and the downstream membership
  this step observed is neither read nor stored any more.

Observed on device across one session, against that implementation: nine configs applied as
tethering was cycled, the epoch advancing on every downstream-membership change and every loss
of positive confirmation, `admitting` reaching true exactly when the state reached `ACTIVE`, and
the epoch correctly *not* advancing on `VERIFYING` -> `ACTIVE`, since regaining confirmation
builds fresh state under the epoch the daemon already retired. Membership is gone as a trigger,
so what a rerun would observe is fewer configs and fewer advances; every other observation in
that list is unchanged.

The TUN reader and classification are also in, and device-verified:

- **`shared/classify.rs`** compares the destination against the exact virtual set
  before attribution, reassembly, or transport dispatch. An exact TCP/UDP port-53
  endpoint is `platform_dns`; a later fragment for a virtual address is a provisional
  `platform_dns` candidate, so reassembly is charged to the principal it will belong to
  rather than to whoever completes it; any other protocol or port to a virtual address
  is dropped without a response or an upstream socket; everything else is attributed by
  family, unless it is link-scoped, in which case it is dropped without an upstream
  socket at all. Seven unit tests, including the fragment, malformed, and link-scoped
  cases, and the private/unique-local destinations that must *not* be caught by the last
  one.
- **`tun_reader.rs`** owns the descriptor, and gates on admission **per packet** rather
  than by starting and stopping the task: a packet Android already queued carries no
  epoch and arrives whether the daemon is serving or not, so the read side is the only
  place that decision can be made. It reports counters when the epoch changes and once
  at exit, never per packet, because the input is attacker-influenced and a report per
  packet would be a flood by construction.
- The reader is cancelled and joined before the session returns, so exactly one owner
  closes the descriptor and no read outlives the session.

Measured on device, injecting with `ping -I testtunN` from root - which is the same
interface-selection primitive Step 2 found, now useful as a test tool:

- while not `ACTIVE`, 79 packets across three epochs counted as `unadmitted` and
  dropped with no classification and no state, and the counters reset per epoch;
- while `ACTIVE`, two ICMP pings to the virtual DNS address were counted as exactly
  `reserved 2`, alongside `ipv4 12` and `ipv6 12` of ordinary traffic. The
  reserved-address branch is therefore device-proven; the port-53 discrimination inside
  it is unit-tested only, because reaching it needs a UDP send bound to the interface
  and the DNS handoff slice will exercise it for real.

Android's own network validation probes the TUN once the agent publishes, which is why
the ordinary counters are larger than what was injected.

The common writer is in as well, in
[`tun_writer.rs`](../../mobile/src/main/rust/vpnhotspotd/src/tun_writer.rs), with the
two backpressure sources the audit separated kept separate: `enqueue` refuses when the
daemon's own queue is full, which is an admission decision the producer refunds against,
while `EAGAIN` on the descriptor is a wait for writability that never re-charges a packet
already accepted. Every dequeued packet is gated on the epoch it was produced under -
the second half of the gate, which catches an old-epoch task that enqueued after a purge
- and then validated, so a producer that built something malformed or oversized cannot
reach a client.

Its queue depth is the kernel's own `TUN_READQ_SIZE`, 500, which `tun_setup` assigns to
the device's `tx_queue_len`. Matching the device means the daemon buffers no more toward a
client than the interface itself would: deeper would add latency the device does not, and
shallower would drop where the device would not.

The reader and writer share one `AsyncFd` behind an `Arc`, since its readiness methods
take `&self`, and both are cancelled and joined before the session returns, so exactly one
owner closes the descriptor and neither a read nor a write outlives the session. Verified
on device: both tasks start, drain and report, and the daemon exits 0.

Since done: while the session continues, the writer answers for every guarded IPv4 packet it
accepts, because it is the only owner that can. What was queued here is not what was written to
the TUN, and the difference is what the Identification window has to be measured from - see
[The Nonreuse Window](#the-nonreuse-window). A successful write is as far as this process can
see; whether a client then received it is not something the daemon observes. That run predates
all of it and does not qualify it.

**The UDP relay is in, and it is the writer's first producer**, so the `#[expect(dead_code)]`
that guarded `Writer::enqueue` is gone - which is exactly what that annotation was there to
force.

- **`shared/udp_wire.rs`** is the strict parse of a client datagram and the construction of
  the reply. Strict rather than forgiving on purpose: the daemon re-originates the datagram
  from a different source address, so a guess about a self-inconsistent one arrives at the
  remote looking authoritative. It verifies the UDP checksum, which is the only place
  corruption can still be seen - past this point the kernel computes a fresh valid one over
  the new source - accepting an omitted IPv4 checksum, which IPv4 permits, and refusing an
  absent IPv6 one, which IPv6 does not. Fragments, extension-header chains, and other
  transports are each their own rejection rather than one lumped drop, because each belongs
  to a different later slice. Nine unit tests.
- **`udp.rs`** is the table. Keyed on the TUN-visible source alone: the generation and family
  the design names cannot vary within one table, since either axis advancing retires all of
  it and the family is the address's own. One unconnected socket per mapping, bound to a port
  up front so the identity it pins exists for its whole life rather than appearing on first
  use, which is what makes its budget charge real rather than anticipated. Permitted remotes
  are recorded per mapping with their own deadlines, charged only once a reply is actually
  possible, so a refused send pays nothing. `IP_MTU_DISCOVER` is reapplied immediately before
  each IPv4 send; IPv6 has no such bit to carry, since no router may fragment an IPv6 packet.
- **The reply path deliberately splits the work.** Each mapping's receive task does no
  packetization and keeps no buffer of its own: it peeks the queued datagram's real length
  with `MSG_PEEK | MSG_TRUNC`, allocates exactly that, and hands it to the table. A fixed
  per-task buffer would have to hold a whole 64 KiB datagram to be correct, and any local app
  can multiply that by the mapping ceiling - 1.8 GB at the measured ceiling. One extra syscall
  per reply is the cheaper side of that trade. Centralizing packetization is also what keeps the
  Identification allocators *shared* - one per session, reached by every producer - which matters
  because two mappings from one client share a reassembly tuple and so cannot each own an allocator.
- **The refund is keyed to the receive task's completion, not to a message from it and not to
  the retirement request.** A `Closed` message would be sent while the task still held its
  `Arc` of the socket, so it proves nothing about the descriptor; the task's own completion
  does, because tokio drops a task's future before its completion is observable. So the
  mapping holds one share of the socket and the task the other, retirement cancels and then
  joins, the mapping is taken out of the table - which drops the last share - and only then is
  the budget refunded. The acknowledgement therefore means the descriptors are actually gone,
  rather than that something asked for them to go. The same protocol runs when a mapping fails
  on its own, so there is one settlement path rather than two.
- **`budget.rs`** is the one admission owner, measured rather than chosen - see
  [Descriptors](#descriptors) for why the identity ceiling is left to the kernel.
- The reply-interface check needed a new field. `ShizukuSessionConfig.upstream_interface_index`
  carries what the app resolves with `Os.if_nametoindex` from `LinkProperties.getInterfaceName`,
  and an *unconnected relay* - UDP and Echo - is refused without it rather than serving
  unchecked, because that check is the whole reason a reissued local port is safe.

  **Two facts, not one.** The resolver and the terminating TCP engine need only the handle:
  both `connect`, so the kernel picks the source and the reply arrives on that connection.
  Reconstructing one combined "upstream" at each use made the index a precondition for all
  four, so a session could not resolve a name during the window between selecting an upstream
  and resolving its interface index - two separate observations in the app. The config is
  therefore decoded once, before publication, into a selected network and an optional relay
  upstream. Three shapes are accepted (neither; network alone; network with index) and every
  other shape is terminal - in particular a *present zero*, on either field. Zero is what a
  truncated or default-constructed message decodes to, and `android_setsocknetwork(0)` means
  the process's own default network, which is exactly the fallback this mode does not have.

Verified end to end on device, against a VPN upstream, which is the case this mode exists for.
Injection used root-side uid-scoped routes into the TUN (`ip rule ... uidrange 0-0 to <resolver>
lookup <table>`), which the daemon's own app-UID sockets cannot match, so there is no loop:

- DNS queries relayed and **answered through the TUN**, 228 bytes each, on both families: three
  over IPv4 and, separately, one over IPv6 against the VPN's own resolver. Confirmed three ways -
  the client received them, `testtunN`'s `rx_packets` rose by exactly the number written, and the
  writer's own report read `written 3 stale 0 rejected 0`. A fourth query, to Google's IPv6
  resolver, was sent but unanswered, which is that VPN having no IPv6 transit to it rather than
  anything here: the relay counted `sent` and no failure;
- one source port reused across two destinations created one mapping with two permitted-remote
  records, which is the endpoint-independent shape;
- `relayed 4 / sent 4 / written 3` with `send-failed 0 df-failed 0 open-failed 0
  foreign-interface 0 unpermitted 0 stale 0`, and the budget returning to `0 of 32500 charged`
  from a peak of 8 with `overrefunded 0`, so every reserve was refunded exactly once. The peak
  accounts for itself exactly: four mappings and four permitted remotes;
- `unroutable 26` against those 4 relayable packets, nearly all mDNS. That count is why the
  link-scoped drop exists at all: before it, the same traffic produced 56 opaque send failures
  and the relay was attempting to put a tethered client's service discovery onto the VPN.

Two gaps in this slice, both deliberate and both ICMP's to close in step 9. `EMSGSIZE` on a
DF-set send is counted rather than translated, so the path-MTU discovery it would have driven
toward the client is not relayed yet; and an expired hop limit is dropped rather than answered
with Time Exceeded. The error queue is already drained on every ICMP error, which is what that
translation will read.

**The virtual-DNS handoff is in, which completes step 7.** It is deliberately not part of the
UDP relay, because it owns no selected-network socket at all: the query goes to the platform
resolver and the answer comes back on a descriptor, so private DNS, caching, and per-network
resolver configuration are kept rather than reimplemented, and a handover has nothing of this
module's to sweep.

- **`resolver.rs`** was extracted from the root mode's `dns.rs` rather than written again. The
  descriptor protocol is subtle enough that a second copy would be a second place to get it
  wrong - `android_res_nsend` returns a descriptor, `android_res_nresult` reads it with
  *synchronous* reads, so the wait is for peer close on a nonblocking descriptor before handing
  it back to the NDK reader - and none of it depends on which UID is asking. The extraction also
  brought `dns.rs` back under the module size target.
- **`virtual_dns.rs`** never cancels an in-flight query to free capacity, which is the rule the
  design argues for at length: cancelling recovers this process's descriptor and not the
  resolver's work, and it destroys the completion signal that made the charge exact. The
  descriptor is held, the answer awaited, and the slot refunded on completion - never on a
  deadline. An answer is discarded by *both* axes: the generation because it may have been
  resolved on a network no longer selected, and the epoch because the client address it would go
  to may no longer mean the same device. Discarding still refunds. The ownership fence sharpened
  what "on completion" means: each query is a task the handoff owns, and the refund follows *that
  task* finishing rather than the answer arriving, because the answer arriving says the answer is
  ready while only the task finishing says the descriptor is back. The one thing that cancels such
  a query is the session ending, where there is no capacity to free and the process is recovering
  what it owns before it exits.
- **`output.rs`** is new and is the reason the DNS answer and a UDP reply cannot corrupt each
  other. The IPv4 Identification allocator has to be shared to be correct: a receiver reassembles
  on source, destination, protocol, and Identification, and *ports are not in that tuple*, so a
  DNS answer and a relayed reply to the same client would collide if each producer owned an
  allocator. It also puts the whole size policy - the DF decision against the floor, source
  fragmentation against the interface - in one function instead of two that drift.
- **The reserve shrank from 256 to 32**, which is the more correct number: the platform's per-UID
  cap bounds the *ceiling*, but the *reserve* should be what this daemon's own DNS can hold at
  once, since the rest of that cap is spent in processes whose descriptors are not this one's to
  hold back.

Verified on device, injecting to the exact virtual endpoints through the same uid-scoped routes:

- a query to `192.0.2.5:53` and one to `[fd00::53]:53` were both answered with a real 228-byte
  response - transaction ID preserved, `ANCOUNT` 13, the root servers named - so both families
  reach the resolver and come back through the TUN;
- an 80-query burst produced **57 answers and 25 SERVFAILs and zero silent drops**. The SERVFAILs
  carry rcode 2 with the question echoed and the ID preserved, which is admission denial behaving
  as the design specifies: refused, not dropped, because a SERVFAIL fits capacity already owned;
- the counters close exactly against each other and against the interface: `dns 82` in,
  `answered 57 + servfail 25` out, `tun output written 82`, `tun egress written 82 stale 0
  rejected 0`, and `rx_packets` up by 82. The query ceiling reads `peak 32, denied 25` and returns
  to `0 of 32`, with `overrefunded 0`;
- `udp relay` stayed all zeros throughout, confirming the handoff never routes through the relay -
  which matters, since the relay would have tried to open an upstream socket to an address the
  daemon itself occupies.

Still outstanding, and belonging to later slices rather than to this one: the remote-ICMP
correlation history [UDP](#udp) describes, which goes with the ICMP slice that would use it, and
TCP port 53 to a virtual address, which is the same principal but needs the terminating engine and
is counted as `undeliverable-dns` until then. Upstream generation was fixed at one here; selected-network
observation and the handover it drives landed in step 10.

### Step 8 Status

Terminated TCP is in, and a real connection completes through it. The client-facing half is
[`smoltcp`](https://crates.io/crates/smoltcp) rather than a stack written here: root mode lets the
kernel do the TCP work through `TPROXY`, which needs netfilter the app UID cannot touch, so an
in-process stack is forced - and hand-writing sequence numbers, windows, retransmission and RTO
would be the surprising choice, not the careful one.

- **`tcp_device.rs`** is the whole adapter, and deliberately thin. Ingress is *pushed*, because the
  ingress task has already read and classified the packet; egress is collected and drained into the
  same writer every other producer uses, so the epoch gate and the final size validation apply to
  TCP exactly as they do to UDP.
- **The MTU it advertises is the downstream floor, not the interface's.** That makes the floor do
  the right thing for TCP with no DF machinery at all: an MSS negotiated from the floor means every
  segment already fits the narrowest downstream, so Android never has to fragment or reject one.
  It is also why terminated TCP absorbs the size mismatch that relayed UDP cannot.
- **Interception is a listen on the destination the client chose.** A SYN is peeked at, a socket is
  opened listening on that exact endpoint, and only then is the packet handed to the stack, which
  `any_ip` lets accept a connection to an address it does not own. A duplicate SYN falls through to
  the stack instead, which reuses the half-open state it already has rather than allocating a
  second flow.
- **The client handshake is not held for the upstream connect; the two run concurrently.** Holding
  the client's SYN would need the stack to defer one it has already been given, which it does not
  offer, so an unreachable destination is a reset after the handshake rather than a timeout during
  it. That is the more informative of the two failures. Either order really happens, though: the
  connect starts from the SYN path, so a remote that greets first can have bytes waiting while the
  client half is still `SYN-RECEIVED`. Those bytes wait - the client pump consumes nothing from a
  half that cannot yet send, keeps its readiness marker, and runs again on the final ACK - so an
  early greeting is delivered once, in order, and an early end of stream is not acknowledged
  before the connection it belongs to exists.
- **`tcp_flow.rs`** is the bounded bidirectional backpressure the design asks for, and it needs no
  byte counter. Two different things do the bounding: a channel's depth bounds how many values may
  be *queued*, while what bounds the payload alive at once is the serial shape of the flow task -
  one `select!`, in one branch at a time, awaiting an acknowledgment before building the next piece.
  A depth-one channel is therefore not a one-buffer bound, because the consumer may hold a dequeued
  chunk while a successor is queued behind it. With that said, each direction's channel is what
  applies the pressure. When the upstream half cannot keep up the
  engine stops draining the stack's receive buffer and the client's window closes; when the engine
  cannot keep up the flow task stops reading the upstream and the *remote's* window closes. Neither
  direction drops data, which is what separates a terminated stream from the relayed datagrams next
  door.
- **The engine's own buffering is one chunk, total.** The shared event channel is drained only when
  that slot is empty, so at most one chunk can be waiting for a socket's send buffer anywhere in the
  engine. The cost is head-of-line blocking of other flows behind one stalled one, which is a real
  limitation and the first thing to revisit if throughput needs it.
- **The flow ceiling is measured, and it is the first genuine memory bound in this mode.** A flow's
  cost is its two buffers, not its one descriptor, so it is charged against a nested ceiling derived
  from `MemAvailable` - readable at the app UID, unlike every sysctl the daemon would otherwise
  want. An eighth of it, divided by two 64 KiB buffers. Measured on the qualified device: 1712
  flows from 1.75 GB available. 64 KiB is the largest window a receiver can advertise without RFC
  1323 scaling, so it is the largest buffer useful against every peer rather than only against those
  that negotiate scaling.

One bug found by device testing and worth recording, because the same mistake is available anywhere
this stack is asked whether a side is finished. Half-close detection asked `may_recv()`, which is
also false for a socket that is merely *listening* - so the first poll after opening a flow read a
brand-new connection as a half-closed one, dropped the upstream write half, and the flow could never
carry a byte. It presented as `opened 2, reset 0, to-upstream 0`: flows opening, upstreams
connecting, nothing moving. That check is now gated on the handshake having actually completed. The
finished-socket sweep was gated the same way at the time and no longer is: a `Closed` socket is finished
whether or not it was ever open, and the gate's reasoning - that a socket which never got there has
nothing finished about it - was wrong even where it was harmless. See
[Outstanding Daemon Work](#outstanding-daemon-work) for why it was in fact harmless.

Verified on device through the same uid-scoped routes, against a VPN upstream:

- **DNS over TCP** to `8.8.8.8:53` returned 230 bytes - a two-byte length prefix and a 228-byte
  answer with `ANCOUNT` 13 - so the handshake, the upstream connect, the client's request and the
  response all work;
- **an HTTP request** to `1.1.1.1:80` returned a 389-byte `301 Moved Permanently` across several
  segments with a clean close, which exercises more than one segment in each direction;
- the counters close **exactly** against what the client saw, in both directions: `to-upstream 79`
  is 19 + 60 bytes sent, `to-client 619` is 230 + 389 received, with `opened 2 closed 2 reset 0
  stale 0 unconsumed 0`, `tun egress written 13 stale 0 rejected 0`, and the flow ceiling returning
  to `0 of 1712` from `peak 1` with `overrefunded 0`.

**Outstanding for step 8, and the first item is a bug rather than a refinement.**

`Event::Upstream` assigns the engine's pending slot rather than appending to it, so a chunk arriving
while the previous one is still partly unflushed **overwrites it and those bytes are gone** - silent loss
in the middle of a TCP stream. It is reachable by an ordinary slow client: once the client's window
closes, `send_slice` returns `Ok(0)` or a partial count, the slot keeps its remainder, and the next chunk
from the same flow replaces it.

The comment on `FLOW_DEPTH` is what hid it. It says one read in flight is "what keeps the engine's
pending slot below at exactly one chunk", which conflates one chunk in the *channel* with one chunk in
the *engine*. A flow's task awaits its send, so it has at most one outstanding - but the engine drains
the shared channel every loop, so the same flow can hand it a second chunk before the first has been
flushed. One in flight does not imply one in hand.

**Fixed, and not by adding a queue.** The slot is safe as long as nothing is taken while it is occupied,
so the engine now declines to read the channel at all until the chunk it holds has reached its client -
`select!` takes a guard on the arm, so this is one condition rather than a new mechanism. The chunk then
stays in the channel, the flow's task blocks on its send, and the upstream's receive window closes, which
is the backpressure the `FLOW_DEPTH` comment already described and was simply not applying here. A
`debug_assert` in the engine states the invariant the guard maintains.

What that costs is honest and bounded: while one client's window is shut, the other flows' events wait
behind it, so the head-of-line blocking the per-flow output queues are for is now stronger rather than
weaker. It cannot deadlock - a client that never reopens its window fails the socket on the stack's own
retransmission timeout, which frees the slot, and a flow that is already gone frees it on the next pump.
Delivering a stream with a hole in it would be worse than delaying one.

The per-flow queue remains worth doing, and the earlier framing of *how* was wrong: appending needs a
bound, and the bound cannot come from the shared events channel, because the engine drains it every loop
whatever depth it has. It needs a per-flow channel the engine can leave unread for one flow while serving
the others - which is the same guard as above, applied per flow instead of globally.

The guard was then verified on both flow kinds, which matters because it sits in the path all TCP uses and
was first proven only through the resolver variant. An ordinary relayed flow to a real upstream gave
`opened 2 resolved 0 closed 2 reset 0` with `to-upstream 38` and `to-client 460`, and `0 of 32 queries,
peak 0` - a useful negative: a relayed flow reserves no resolver slot, while a virtual-address flow does.
Both closed through the client-closes-first half-close, so that edge is now device-proven too.

Still outstanding: a client that **resets** mid-stream, and a **simultaneous** close. Neither is arrangeable
with the tools on the device - `nc` closes cleanly and offers no way to send a reset or to time a close
against the peer's - so proving them needs either a purpose-built client or a second machine, which is the
real-client testing deferred to the end.

**TCP port 53 into the resolver handoff, with the one decision it needs settled in advance.** The
engine already listens on whatever destination a client chose, so a DNS-over-TCP connection to a
virtual address arrives as an ordinary flow; what differs is that it must be served by
[`resolver.rs`](../../mobile/src/main/rust/vpnhotspotd/src/resolver.rs) instead of by an upstream
connect, reading the two-byte length prefix RFC 1035 section 4.2.2 puts in front of each message and
writing the answer back the same way.

The decision worth recording is how such a flow is charged, because the obvious answer is wrong. The
nested resolver ceiling exists to stay under the platform's per-UID limiter, and a flow that called
`resolver::query` directly would bypass it - the accounting lives in the ingress task and the flow's
task cannot reach it. Asking the ingress task per query would cost a round trip on every message. So
**a DNS-over-TCP flow reserves one query slot for its lifetime and is refused when none is free**,
which bounds concurrent TCP DNS connections to the same ceiling as concurrent queries. That wastes a
slot while a connection is idle and caps such connections at that ceiling; both are acceptable for a
relay serving a handful of tethered clients, and neither can starve UDP DNS, which draws on the same
reserve rather than a separate one.

Half of that has since changed and the half above stands. The *logical token* is still per flow, taken
when the connection opens and refused when none is free. What is not is the rest of the exchange: the
descriptor record and the bytes are admitted per query, through the ingress owner, because an idle
connection owes neither and because the length a client announces has to be charged before that message
is stored. The round trip that argued against it is one the transport already makes - it asks the same
owner to admit its query and tells it when the answer has been delivered - and it is what fixes each
query's `Network` and stamp at acceptance rather than at the flow's. What a transport asks over is the
depth-one control pair built with its flow, so the round trip costs no per-query allocation either.

**Done, and the slot rule above is what it was built to.** [`tcp_dns.rs`](../../mobile/src/main/rust/vpnhotspotd/src/tcp_dns.rs)
is deliberately interchangeable with the splice: the engine opens the flow the same way, hands over the
same receiver and gets back the same events, and only where the answers come from differs. Nothing
assumes one read is one message, because TCP offers no such thing. The generation and admission semantics
around it are done too, host-tested through the daemon's own owners - see
[Outstanding Daemon Work](#outstanding-daemon-work) for what that list no longer contains, and the
device matrix below for what it still does.

Verified on device: three DNS-over-TCP connections to the virtual address, each returning a 230-byte
answer - a two-byte prefix and a real 228-byte reply from the platform resolver, with the query ID
echoed. That path was `undeliverable-dns` and dropped before this. That run is older than everything
described above: it proves the transport carried real answers, and it qualifies none of the generation,
admission or quarantine behaviour that came after it.

**A leak showed up in the same run, and only the budget counters could have shown it.** The first
attempt answered all three queries correctly and still reported `closed 0` with `3 of 32 queries` and
three flows charged at exit. The task returned quietly when the client half-closed, so the engine never
learned the flow had ended and refunded neither reservation - and for a resolver flow there is no
upstream socket whose loss would say it instead, which is exactly what makes this different from the
splice. Its ending is now the task's own completion, which the engine joins and settles, the same
discipline [`reply.rs`](../../mobile/src/main/rust/vpnhotspotd/src/reply.rs) follows for the same
reason. After the fix: `opened 3 resolved 3 closed 3 reset 0`, and `0 of 32 queries, peak 1` with
`overrefunded 0`, so the slot is taken and returned exactly once per flow. That run predates the
ownership fence, which replaced the `Closed` message it verified with the join described in
[Outstanding Daemon Work](#outstanding-daemon-work); the counters it read are the same ones.

### Step 9 Status

Started, and it closes the two gaps step 7 left. The daemon can now originate an ICMP error toward a
client, which it previously could not do at all - so a forwarding decision it makes is something it can
explain rather than a silent drop.

**The open question from step 7 is resolved, and the answer was already in the design.** Fragmentation
Needed is only useful with the real path MTU, and `EMSGSIZE` from `sendmsg` does not carry it;
`IP_MTU` needs a destination and these sockets are unconnected on purpose. But the kernel calls
`ip_local_error` with the MTU on a local DF-set refusal, which lands on the socket's error queue as
`ee_info` - and the error queue is already enabled, because the design required it for unrelated
reasons.

- https://android.googlesource.com/kernel/common/+/refs/heads/android16-6.12/net/ipv4/ip_output.c#998
- https://android.googlesource.com/kernel/common/+/refs/heads/android16-6.12/net/ipv4/ip_sockglue.c#469

- **`shared/icmp_error.rs`** originates rather than translates, which is why the source address is the
  interface's own and the hop limit is the local origin value. Translating an error a *remote* sent is a
  different problem with a different correctness argument and is not in this module. Quotes are
  truncated and never fragmented, bounded by 576 for IPv4 (RFC 1812 4.3.2.3) and 1280 for IPv6 (RFC
  4443 2.4c), because an error that needs reassembling to arrive defeats its own purpose. Four unit
  tests, including that the reported MTU reaches the wire in both families' different field layouts.
- **The gateway address is configured, not assumed.** `ShizukuSessionConfig.gateway_addresses` carries
  the interface's own address per family, and without the matching family the error is not sent at all
  rather than sent from something else.
- **A missing MTU means silence.** If the error queue has no entry, or one about something else, the
  relay counts `unreported` and says nothing. A wrong MTU is cached by the client for minutes, so too
  small costs throughput for that whole window and too large keeps the black hole open while looking
  fixed - silence is the better failure.

Verified on device, against a VPN upstream whose MTU is 1320:

- **Fragmentation Needed works end to end.** A 1440-byte DF-set UDP datagram produced `too_big 1,
  reported 1, unreported 0` with one packet written, so the send failed, the MTU was recovered from the
  error queue, and the error reached the client. This is the black hole step 7 documented, now closed.
- **Time Exceeded works end to end too**, though it took a second attempt at injecting one. `nc` cannot
  set a TTL and `ping` is not UDP, so `traceroute -f 1 -m 1` was tried first and delivered no probe the
  relay ever saw. What works is a **route attribute**: `ip route add ... hoplimit 1` sets the initial
  TTL for locally originated packets using that route, which any ordinary `nc` send then inherits.
  Result: `relayed 1, expired 1, reported 1, unreported 0` with one packet written - and `sent 0` with
  no mapping opened, so the expired datagram was refused *before* any upstream socket existed, which is
  the ordering a router owes.

**Echo is implemented, and the kernel chose its shape.** An unprivileged ping socket overwrites the
identifier of everything sent through it with the socket's own bound port, so every session on one socket
wears the same identifier and it cannot tell them apart; the sequence is passed through untouched. So the
allocated field is the **sequence**, not the identifier - the mirror image of the root mode's NAT66 table,
which allocates the identifier because it rewrites packets it owns outright. A session is keyed by
`(remote, allocated sequence)` and carries the client's own pair to restore on the way back.

- https://android.googlesource.com/kernel/common/+/refs/heads/android16-6.12/net/ipv4/ping.c

**Substituting the sequence is only safe because Android's inner IPv4 NAT does not look at it**, and that
is worth stating explicitly because the whole approach rests on it. A tethered IPv4 client's ping reaches
the TUN already source-NATed to the upstream address, so the daemon cannot tell clients apart and the
return path is conntrack's to reverse - and `icmp_pkt_to_tuple` builds its tuple from **type, code and
`un.echo.id` only**. The sequence is absent from it. So rewriting the sequence is invisible to the NAT,
while restoring the identifier byte for byte is what lets the NAT deliver the reply to the client that
asked. `nf_conntrack_icmp_packet` also refreshes rather than deletes on the first reply, so many
outstanding sequences under one identifier remain one conntrack entry.

- https://android.googlesource.com/kernel/common/+/refs/heads/android16-6.12/net/netfilter/nf_conntrack_proto_icmp.c

The client's own sequence still has to come back, because `ping` matches replies on `(id, seq)` - so both
halves of the pair are restored: the identifier for the NAT, the sequence for the client.

- **One socket per family and generation**, not per session: the kernel demultiplexes replies on the
  identifier alone, so a socket per session would buy nothing and spend a descriptor each. Echo is
  therefore optional independently per family, exactly as the [ICMP](#icmp) table says - a family whose
  ping socket will not open is a family without Echo, not a broken session.
- **The socket is bound to port zero up front**, which is what makes it able to receive at all rather than
  merely tidy: `ping_get_port` assigns a free non-zero identifier on such a bind, and that identifier is
  what the kernel matches replies against.
- **The request is written with a zero checksum and a zero identifier**, because the kernel fills in the
  first and overwrites the second. Writing either would be writing a value that never reaches the wire.
- **A session is consumed by its first reply.** A duplicate, a reply to a session that timed out, and a
  reply from a remote never sent to are then indistinguishable, all counted `unmatched` and dropped -
  which is the safe direction to be wrong in. The remote being *part of the key* is what makes the address
  filter structural rather than a comparison that could be forgotten.
- **The session timer is 60 seconds**, the [Timers](#timers) floor taken exactly: nothing recommends longer
  for a ping, and since a session is consumed by its own reply this only bounds how long an unanswered one
  occupies a sequence.
- **Echo Replies go through the same size policy as UDP**, so `output.rs` now owns one `emit` that both
  call. That is reachable rather than theoretical: the downstream floor can shrink between a request and
  its reply, so a ping that fitted when it was asked for may not by the time it is answered.

**One correctness fix fell out of building it, and it applies to UDP too.** The error queue is FIFO and
holds both the kernel's own local refusals and errors routers sent. Translating whatever is at the head into
a Fragmentation Needed could report a real path MTU belonging to a *different* destination, since one socket
serves many - and, worse, a router's error sitting at the head hides the local refusal behind it for as long
as it stays there. So the queue is drained through rather than peeked at, and only an entry whose origin is
`SO_EE_ORIGIN_LOCAL` is returned. Reading one entry and checking its origin fixes the first half and leaves
the second, which is exactly the state the device caught this in.

Attribution rests on that origin check because the kernel offers nothing better: `ip_local_error` is passed
`inet_dport`, which is zero on an unconnected socket, and `ip_recv_error` fills the message address only
when that port is non-zero. So a local refusal reports *no* address, and the only thing tying it to a
destination is that the code path which saw the send fail is the one that drains it.

This matters more for Echo, where one socket serves every client, but it was already wrong for a UDP mapping
serving several remotes - the step 7 proof passed because it ran against an empty queue.

Every kernel behaviour this rests on was read out of `ping.c` rather than assumed, and all of it holds:

- `ping_get_port` with `ident == 0` walks for a free identifier, skips zero explicitly, and assigns it to
  `inet_num`, so binding to port zero is what gives the socket an identifier to be matched on;
- `ping_v4_sendmsg` sets `un.echo.id = inet->inet_sport`, takes `un.echo.sequence` from the caller
  unchanged, and zeroes the checksum, which `ping_v4_push_pending_frames` then folds in - so the
  identifier really is unavailable and the sequence really is the only field left;
- `ip_cmsg_send` is called and its cookie reaches `ip_append_data`, so the per-message TTL is honoured
  rather than silently replaced by the socket default;
- `ping_recvmsg` calls `ip_cmsg_recv` for IPv4 and both `ip6_datagram_recv_*_ctl` for IPv6, so the
  received hop limit and interface index are available in both families;
- `ping_supported` permits only Echo Request with code zero, which is exactly what this builds - and is
  independent confirmation that rejecting a non-zero code in the parse is right rather than pedantic;
- `ping_init_sock` is where `ping_group_range` is enforced, at socket *creation*. Step 6 already proved
  creation succeeds at the app UID, so the bind cannot fail for permission reasons.

Verified on device, injecting with `ping -I testtunN` from the root shell. Echo needs no route trick, unlike
UDP: binding to the interface is enough, and the daemon's own sockets are bound to the upstream instead, so
there is no loop.

- **Echo works end to end in both families.** `ping -I testtunN -c 3 8.8.8.8` returned three replies with
  `icmp_seq=1,2,3` intact, and IPv6 likewise against the upstream's own resolver. That the client's own
  sequence numbers come back is the whole substitution proving itself: the daemon sent none of those
  sequences on the wire.
- **The hop-limit control message is honoured**, settled by the A/B rather than by a traceroute, because
  relaying the Time Exceeded a *router* sends is still outstanding. `ping -t 64` was answered and `ping -t 2`
  was not - the daemon decremented it to 1 and the first upstream hop discarded it - while both counted
  `sent 1`. Had the control message been ignored, the request would have left at the socket default of 64 and
  been answered, and no counter would have shown the difference.
- **Time Exceeded and Fragmentation Needed both fire for Echo**: `ping -t 1` produced `expired 1` with a
  packet written, and a DF-set `-s 1400` request against a 1320-byte upstream produced `too_big`, `reported`,
  and a real MTU. Final counters `sent 5 written 4 expired 1 too-big 2 reported 3 unreported 0 unparseable 0`
  account for every packet exactly.
- The injecting `ping` does not *display* either ICMP error, so the evidence for those two is the counters
  plus `rx_packets` rising by exactly one per error. That is a property of feeding a root shell's own socket
  from the interface it is bound to, not of the packets: every one of them passed the writer's validation
  (`tun output ... unwritable 0`).

Two bugs only a device could have found, and neither was in the Echo logic:

1. **`peek_length` cannot size a read from a ping socket.** `ping_recvmsg` is its own implementation: it
   returns `min(skb->len, len)` and treats `MSG_TRUNC` as an output flag, where `udp_recvmsg` reports the true
   datagram length. So the one-byte probe reported one byte, every reply was truncated to below an ICMP
   header, and all of them landed in `unparseable` - `sent 8 written 0`. Ping sockets now hold one
   full-datagram buffer for the life of the task, which is affordable precisely because there are two of them
   per session rather than one per mapping.
2. **The error queue is FIFO and only its head was being read.** A router's error about an earlier packet
   arrives first and stays there, hiding the local refusal behind it - so the second Fragmentation Needed
   test failed while the first passed, purely on ordering. The queue is now drained through, and only the
   local refusal is returned. The oversized control-message buffer that surfaced this (`ENOBUFS`, because a
   router-origin error carries receive metadata a local refusal does not) was the same bug wearing a
   different hat.

**The second one was already wrong for UDP**, which is why the step 7 proof passed: it ran against an empty
error queue. Both relays share the fix, and UDP was re-proven after it - a 17-byte root NS query relayed and
answered (`sent 1 written 1`, 228 bytes reaching the client) and a 1440-byte DF-set datagram producing
`too-big 1 reported 1 unreported 0`.

**Ingress reassembly is in, for both families, and it closes a gap that was total rather than partial.** The
relays parse whole datagrams and nothing else, so before this every fragment a client sent was counted and
dropped - large pings, UDP past the downstream MTU, anything that fragments, lost completely rather than
degraded.

- **`shared/reassembly.rs`** hands back a datagram with the fragmentation *removed*: IPv4 flags and offset
  cleared and the header checksum recomputed, or the IPv6 Fragment header spliced out and its next header
  promoted. That is what lets the existing strict parses run on the result unchanged instead of each learning
  a second shape, and it is why the dispatch had to become callable twice - once for the packet that arrived,
  once for the datagram whose last fragment it turned out to be. The second pass is the *same* path, so a
  reassembled datagram cannot be admitted by rules that drifted from the ones a whole one meets.
- **Overlaps discard the whole datagram**, per RFC 5722, rather than being merged. An overlap is a broken
  sender or an attempt to make two readers assemble different bytes from one exchange, and there is no third
  case worth serving.
- **The key follows each family's own specification**: source, destination, protocol and Identification for
  IPv4, and source, destination and Identification only for IPv6, whose Fragment header carries the next
  header instead. The field is zero there rather than holding a value RFC 8200 says not to key on.
- **Fragment zero's own header is what gets kept**, so the reassembled datagram carries the options and hop
  limit the client actually sent. A context whose byte range completes without it has no header to speak from
  and is dropped rather than reconstructed.
- **The byte ceiling is Linux's own `ipfrag_high_thresh` default**, 4 MiB - the kernel solving this exact
  problem on this exact device - clamped to the dataplane's measured memory share so a smaller device commits
  less. The charge is *buffer growth*, not fragment size, which is the difference that matters: one fragment
  at a high offset opens a whole span, and charging its 8 bytes would miss that entirely.
- **The timer is 60 seconds**, RFC 8200's requirement for IPv6 and within what RFC 791 allows for IPv4, so one
  number serves both. Linux uses 30; the longer bound is taken because this sits behind a downstream link
  whose retransmissions the daemon never sees.

Verified on device, and the first target choice was wrong in an instructive way: 8.8.8.8 over this VPN answers
a 56-byte ping and *no* large one, so the initial run looked like a reassembly failure. The control settled it
in one step - a 1400-byte ping to 8.8.8.8 fails with the daemon out of the path entirely, while 2000 bytes to
the upstream's own resolver succeeds either way.

- **2000, 4000 and 8000-byte pings all answered through the TUN**, in both families. `reassembly ... held 15
  completed 9 malformed 0 overlapping 0 denied 0`, peak 8008 bytes, and `charged 0` afterwards.
- **The reply path source-fragments too**, exercised here for the first time: the 8000-byte reply left as six
  fragments, and `tun output ... written 23 unwritable 0` means every one passed final validation.
- **The timeout and its Time Exceeded are proven precisely.** `tc netem loss 45%` on the TUN drops individual
  fragments where `iptables` cannot - IPv4 fragments locally originated packets *after* the netfilter hooks,
  so `-f` never matches them, and this kernel has no `frag` match module for the IPv6 equivalent. Six lossy
  8000-byte pings gave `expired 6 headless 2` with `fragments-expired 4` and exactly four packets written: the
  two contexts that never received fragment zero were correctly silent, and the four that had it were
  answered. Every byte was refunded.

**Errors a remote sends are now repeated for UDP**, for the two types whose correlation the relay can already
prove. A repeated error's source is the **router's own address**, not the interface's, and that is the point
rather than a detail: an error the daemon originates comes from the interface because the daemon decided, while
a repeated one has to come from whoever decided instead, or every hop on the path collapses into the gateway
and a traceroute through the hotspot stops there.

- **Two types, and the line between them is what can be correlated.** A path MTU and a hop-limit expiry are
  properties of the route to an address, and the mapping already records which addresses it sent to, so a
  permitted remote is proof that the error describes traffic this daemon really produced. Destination
  Unreachable and Parameter Problem are claims about one specific datagram, and repeating those safely needs
  the byte-bounded send history [UDP](#udp) describes - without it a remote could name a datagram that never
  existed and have the daemon tell a client its flow had failed. Those are counted `untranslated` and dropped.
- **The two families never share a numeric path.** ICMPv4 type 3 is Destination Unreachable and ICMPv6 type 3
  is Time Exceeded, so the family comes from the socket the error arrived on rather than from the message, and
  the unit tests assert that exact collision does not mistranslate.
- **Values no path could carry are refused, separately from types not yet carried.** An MTU below the family's
  minimum - 68 for IPv4, 1280 for IPv6 - is `implausible`, as is Time Exceeded code 1, which describes the
  *remote's* own reassembly rather than anything on the way there. A client that cached a hostile MTU would
  stop sending anything useful for as long as it kept it, so silence beats passing it on.
- **The quote is a reconstruction and says so.** The datagram was not retained, so the client's addresses,
  ports and protocol are exact - those are what a receiver matches an error to a socket on - while the hop
  limit is the error's own and the payload is empty. RFC 792 asks for eight bytes of payload; inventing eight
  would be worse than sending none, because a client that compared them would find them wrong.

Verified on device. A UDP datagram injected with TTL 2 leaves the daemon at TTL 1, so the *second* upstream hop
returns the Time Exceeded: `translated 1` with `untranslated 0 implausible 0 unpermitted 0` and exactly one
packet written. The path was checked first without the daemon in it, which is what made the result readable -
this upstream answers at TTL 1 and 2 and is silent beyond, so only one of the three injections could have
produced an error at all. There is no `traceroute` on the device, so the injection is a route attribute
(`ip route ... hoplimit 2`) rather than the tool.

**Finding: the reply task had never actually drained an error.** Its error handling was written from the first
slice onward and looked right, but it waited on `AsyncFd::readable()`, and tokio maps `Interest::READABLE` to
`Ready::READABLE | Ready::READ_CLOSED`, which does not include `Ready::ERROR`. A pending ICMP error raises
`EPOLLERR` and nothing else, because `datagram_poll` adds `EPOLLIN` only when there is data in the receive
queue. So the task never woke for an error: one sat in the queue until unrelated traffic happened to arrive,
which is indistinguishable from the remote never having sent it. Both socket kinds now register
`Interest::READABLE | Interest::ERROR` and wait on both. Nothing before this slice depended on the drain
happening, which is why it went unnoticed - the path-MTU work reads the queue from the send path instead.

**The send history and the correlation rule are in, and what they decide is now one testable policy.** The
distinction they encode is what a claim is *about*, not how much the daemon trusts it:

- **`Correlation::Address`** means this client sent to that address. It carries a claim about the *route* -
  Packet Too Big, Time Exceeded - and the permitted-remote set already establishes it.
- **`Correlation::Datagram`** means this client sent that exact datagram, with the hop limit it used. It
  carries a claim about *one datagram*, which is what Destination Unreachable is, and only
  [`shared/send_history.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/send_history.rs) can establish
  it. Offered address proof for a datagram claim, the answer is `Uncorrelated` - a statement about the
  evidence rather than about the error, so the same error with a matched send behind it is repeated.

The history holds no payloads. A record is a destination, a length, a digest and the client's own hop limit, and
the error queue hands the offending bytes back, so a digest recognises them without ever having stored them.
Fixed-size records are also why a byte bound and a count bound are the same bound here.

Its lifetime is 60 seconds, and that is derived rather than picked: the longest a hop may legitimately hold a
datagram before complaining about it is RFC 8200's reassembly limit, so nothing later than that can honestly be
attributed. Absolute and never refreshed, per [Timers](#timers) - later traffic on the mapping says nothing
about whether an error for *this* datagram can still arrive.

**Correlated translation ends at the first resolution, whatever it was**: a match, a miss, an ambiguity, an
expiry, an eviction. That is deliberately blunt, and it costs nothing real, because a UDP traceroute uses a
fresh source port per probe and each hop's error therefore lands on a mapping of its own. It bounds what a
remote can extract to one answer per mapping and makes the history self-limiting. Two consequences worth
naming: a client that retransmits an identical datagram - a DNS query, typically - makes its own error
ambiguous, and an eviction retires the whole history rather than forgetting one send, because a history with a
hole in it cannot tell "never sent" from "no longer remembered".

Forgetting is reported as `Spent` rather than `Untracked`, and that distinction is load-bearing: "I have records
and yours is not among them" is evidence of an error about a datagram nobody sent, while "I no longer remember"
is evidence of nothing. Reporting the second as the first would make a counter meant to surface forged errors
climb on ordinary idle traffic.

Parameter Problem stays out even with datagram proof, and correlation is not what it lacks: its pointer names a
byte of a header the daemon rewrote, so repeating it would point the client at the wrong offset. It needs a
pointer mapping.

**Destination Unreachable is repeated too, and the device corrected the plan on how.** The design called for a
unique match against the offending datagram. That is not reachable for a conforming router: RFC 792 requires
only the offending IP header plus eight bytes, which for UDP is exactly the UDP header - and the kernel strips
that header before queueing the error, so the payload handed back is **empty**. A digest of the payload can
therefore never match, and the first device run said so precisely: `unsent 1`, meaning an error arrived and the
history reported that nothing described it.

So how much a match proves now scales with what the router chose to quote:

- an **empty quote** is matched to the destination endpoint. That is still real proof, and stronger than the
  address filter, because a record is what says this client sent to that address *and port* - a forged error
  need only name the daemon's local port to reach the socket and may claim any remote port it likes;
- a quote **carrying payload** is additionally matched on the first eight bytes, which identifies the datagram.
  Eight because that is what RFC 792 guarantees; comparing more would be theoretically stronger and practically
  useless, since no conforming router need return it.

Demanding datagram proof from a minimum-length quote would have refused every error a conforming router sends,
which is the shape of mistake only a device catches. Two datagrams sharing an eight-byte prefix are reported
ambiguous rather than falsely distinguished.

Verified on device with the same test that had failed: five destinations probed on a closed port, and Quad9
returned the port unreachable. `translated 1` with `unsent 0 ambiguous 0 implausible 0`, one packet written -
against `unsent 1` and nothing written before the fix. The other four destinations are silent, which is why the
probe uses five: on this upstream most hosts drop rather than answer, and the relay's own counters are what
identify which one responded.

The history is bounded structurally rather than charged: at most eight fixed-size records per mapping, against
a mapping ceiling that is itself measured. So it can never deny admission to anything a client can observe,
which is what the Resource Policy asks of optional state - and the plan's global oldest-first eviction ladder
is not needed, because there is no shared pool to run out of. Eight is a behavioural bound with a benign
overflow: correlated translation ends at the first resolution anyway, so the history only spans a mapping's
sends up to its first error, and overflow retires the history rather than truncating it.

**Echo repeats a remote's errors too, and its correlation comes from a different place than UDP's.** A ping
socket's errors name no destination at all - `ping_err` passes no port to `ip_icmp_error`, so `ip_recv_error`
reports no address - but the kernel keeps the offending *Echo header*, and the sequence in it is the one the
daemon substituted. A sequence matching exactly one live session is therefore proof this daemon sent that
request; more than one is ambiguity, answered with silence. The session is consumed either way, because an error
about a request means its reply is not coming and holding the session open would reserve a sequence for
something that will never arrive.

That asymmetry is worth stating plainly: UDP correlates on a destination the kernel reports, Echo correlates on
a sequence the daemon chose. Both end at one answer per piece of state, and neither infers anything from the
socket the error arrived on.

**Verified on device, and this is the first proof a client displayed rather than a counter.** `ping -t 2` and
`ping -t 3` through the TUN reported two *different* upstream routers; their addresses are redacted.

That is a working traceroute through the hotspot, and it demonstrates three things at once that counters cannot:
the error carries the router's own source address rather than the gateway's, the rebuilt quote is faithful enough
for the client's `ping` to match it to its own probe - note `icmp_seq=1`, so the client's identifier and sequence
were restored correctly - and an ordinary ping still works alongside it. Counters agreed: `translated 2` with
`unmatched 0 ambiguous 0 implausible 0 untranslated 0`, `written 4 unwritable 0`.

**The bounded extension-header walk closes the slice.** Every transport parse expects its header at a fixed
offset, so any IPv6 extension header would otherwise have the packet counted and dropped. The walk removes the
chain and promotes the transport, which is the trick reassembly already uses: hand the strict parses a shape they
understand rather than teach each of them a second one.

Removing rather than preserving is forced. Egress leaves through a datagram socket, so the kernel builds the
IPv6 header and there is nowhere to carry a chain - the same reason the source address changes. Hop-by-Hop and
Routing options are for hops along the way and mean nothing at a relay that re-originates; **Destination Options
are a real loss, and this is where that is written down.**

Two chains are refused rather than walked, and both are counted apart from a malformed packet because they are
well formed and unwelcome: a Routing header with segments left is source routing, which RFC 5095 deprecates and
which a relay must not perform on a client's behalf, and a Hop-by-Hop header anywhere but first breaks RFC 8200's
ordering. A chain longer than the six extension headers RFC 8200 defines is repeating one, which is what a chain
built to be expensive to walk looks like.

A Fragment header ends the walk instead of being removed, and the chain in front of it is still stripped, so what
comes back is exactly what reassembly expects. That is why one read now takes up to **three** passes, and the
bound is the number of wrappings a packet can carry rather than a guess: strip the chain before the Fragment
header, reassemble, then strip whatever chain sat *behind* it in the fragmentable part where RFC 8200 allows one.
Each pass strictly unwraps, so nothing loops, and a packet still asking to be unwrapped after three is one no
conforming sender produces.

Unit tested against the real parses - a chain is only accepted if `udp_wire::parse` then reads the datagram
back - but **not device-proven, and it cannot be from here**: no tool on the device exposes `IPV6_DSTOPTS` or
`IPV6_RTHDR`, so an extension-header packet cannot be injected from a shell. What the device did confirm is that
the three-pass dispatch changed nothing else, since it now runs for every packet: a full regression showed echo
in both families, a repeated router error, IPv4 and IPv6 reassembly and UDP DNS all working, with `extended 0
chain-refused 0` and every failure counter at zero.

With that, step 9 is complete apart from Parameter Problem, which stays out deliberately - its pointer names a
byte of a header the daemon rewrote, so it needs a pointer mapping rather than correlation.

### Step 10 Status

The handover is in, end to end, and the two sides of it landed together because neither
is testable without the other: the app had no way to change the selected network, and the
daemon had nothing to sweep.

- **The app observes its own default network** - `Upstreams.appDefault`, which is
  `registerDefaultNetworkCallback` and therefore per calling UID: a VPN when Android applies one
  to this app, and the ordinary per-UID default when none does. Deliberately *not*
  `Upstreams.primary`, `Upstreams.default`, or the custom-interface regex behind them. Those
  choose where *root* mode sends tethered clients, and root mode can send them anywhere because it
  writes the routes itself; this mode makes a different promise, which is that tethered clients
  share whatever egress Android has already applied to this app. That is a product decision about
  what the mode is rather than an inability to bind - an app UID can bind a socket to any
  accessible handle it names - so `service.upstream`, `service.upstream.fallback` and the regex are
  root-only and cannot move a rootless session's egress. Every change increments
  `upstream_generation`, including a `LinkProperties` change on the same `Network`, because a handle
  survives one that can invalidate the state pinned behind it. A new `onAvailable` publishes null
  before the successor's link properties and unblocked status arrive, so a retired default is never
  exposed during a handover, and a blocked or lost network is null rather than the last good value.
  `BootstrapConfig.upstream_network` is gone; what is left is `probe_network`, which only the debug
  egress probe reads, so there is exactly one path by which the dataplane learns its egress.
- **Recursion is rejected by interface name**, not by `Network` identity, because the check
  also has to hold before the agent publishes, when there is no session `Network` to compare
  against. It is unreachable through this source - a network with no `INTERNET` capability is never
  anyone's default - so it stands as the assertion that keeps it that way.
- **The purge is the writer's gate, not a second mechanism.** Every packet already carried
  the epoch it was produced under; it now carries the generation too, and the ingress task
  publishes the new pair *before* it sweeps. Everything the retired state left in the
  writer's queue is then dropped at dequeue, while the terminal packets the sweep writes
  carry the new pair and leave. Draining the queue explicitly would have needed a barrier
  the writer task cannot offer, for the same outcome.
- **A swept TCP flow with a remote endpoint resets its client, and closes its upstream with
  `SO_LINGER` zero.** The reset is emitted by aborting the client-side socket and polling once
  before the flow's socket is removed, because removing it is what would take the reset with it.
  A flow with no remote endpoint - still listening, or already closed - is aborted silently and
  counted as no reset, so the reset figure is what really went out rather than a flow count. The abortive
  close needs the engine's own sweep token rather than the flow's: a flow cancelled because
  it finished must close ordinarily, and only a sweep means the network is being left.
- **An upstream connect in flight is cancelled with the flow.** It was previously awaited,
  and an unanswered SYN is bounded only by the kernel's connect timeout - long past the
  60-second deadline the acknowledgement has to meet.
- **A resolver answer whose generation was swept now returns SERVFAIL.** Its client's
  transport is untouched by a handover and still waiting, so it is owed the one terminal
  packet a sweep writes. An answer discarded because the *epoch* advanced stays silent,
  since the address it would go to may name a different device by then.
- **An update that cannot be carried or confirmed ends the session**, which it previously
  did not: the observer's exception was swallowed by its supervisor scope and the session
  ran on believing it owned a dataplane. `retire` also runs under `NonCancellable` now,
  because it is called from observers inside the scope it cancels - without that, cancelling
  the scope abandoned the preference clear, the agent's destruction callback and the request
  release, which is exactly the system state it exists to remove.
- **Losing the daemon ends the session promptly.** The app reads the daemon's stream for the
  session's whole life rather than only while a config is in flight, so a child that stops
  answering is noticed when it happens instead of at the next config's deadline. A config round
  trip is raced against that same signal rather than only awaiting an acknowledgement, because the
  reader can fail only the round trips it finds in flight: one begun after the stream ended - the
  ordered stop's own "stop admitting" is exactly that - would otherwise wait out the full
  `control_result_deadline` for an answer no one is left to send, keeping `testtun` published and
  tethering pinned to a dead dataplane for a minute. The deadline still covers what it was written
  for, a child that is still reading its socket and never answers.

**Unexpected background failures are now structured reports.** Everything the app-UID dataplane
knew was previously a line of stdout. `ShizukuDaemonFrame` now carries either an acknowledgement
or a `DaemonErrorReport`, and the app surfaces them exactly as it does the root daemon's.

The two reporters are deliberately *not* one. Root keeps master's shape unchanged - one coalescing task
behind a process-global `OnceLock` channel, installed for the daemon's single control conversation, keyed
on `(context, kind, errno, file, line)` and drained once at the end. The app session needs something root
does not: a reporter it owns, whose finish is part of the session's own result and whose flush is bounded
by its own writer queue. So it gets a `ReporterRegistry` holding a `Weak`, an owned `ReporterGuard` per
session, and its own `SiteCoalescer` keyed on the call site. Dispatch is registry-first with no
fall-through, so an app-session report can never reach root's channel and a root report can never reach a
session's. Folding them together would have meant changing root's behaviour to suit a mode it does not
run. What becomes a report is deliberately narrow, because packet input is
attacker-influenced: a packet the daemon built and cannot send, a socket it cannot open, a
send that failed for no expected reason, a swept socket it could not close abortively, and a
worker whose own I/O or task failed. Everything a client can drive at will stays a counter,
and a peer that merely reset or timed out is one line per record rather than a report.

Coalescing since moved to the producer's side of the queue, in
[`shared/reporter.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/reporter.rs): a queue
in front of it would let one forged packet per report allocate a report nobody drains. The
task that remains only closes a coalescing window, and the session cancels, joins and flushes
it, so an undelivered report becomes the session's own failure instead of a stderr line.

Both gaps this slice left have since closed, and the decisions the document declined to make were
made rather than deferred:

- **The mode has a UI and a foreground service, and the debug broadcast receiver is gone.** One
  global row on the tethering screen rather than one per interface, because this mode owns no
  downstream: it publishes an upstream for Android's system-tethering controls under **Manage**.
  Local-only hotspot and repeater remain separate root-mode features. This mode never cycles system
  tethering for the user. `ShizukuTetheringService` supplies the process lifetime, and it is the
  single ordered command path in both directions - which is what gives the authorization and startup
  window a state of its own instead of rendering it as off and accepting a second start. It adds
  nothing to the shared notification: it registers with `ServiceNotification` exactly as an
  interfaceless service already does. Every row label is terse status - Starting, Ready, Checking,
  Active, "System tethering is not using this connection", Stopping - and never an instruction.
- **A foreign TestNetwork holding the upstream is a terminal collision, decided with ordinary
  reads.** No privileged widening was needed and the earlier claim was simply wrong: ordinary
  `getNetworkCapabilities` enforces `ACCESS_NETWORK_STATE` alone, has no ownership or
  restricted-network gate, and preserves transport types through its sanitizer - the
  location-information one on Android 13-16 and the broader sensitive-information one on 17, which
  redacts more UID, specifier, administrator and underlying-network detail but not transports. So
  `TRANSPORT_TEST` is readable for a live network this app cannot use at all. The classifier is
  therefore three-way: `TRANSPORT_TEST` is a collision, a confirmed non-TEST upstream is
  `RESTART_REQUIRED`, and null - a network that disappeared between tethering naming it and the read
  - stays `VERIFYING`, which also advances the epoch as any other loss of positive confirmation
  does. Ownership itself is still identity against the exact `Network` this session published, never
  a capability read. The same ordinary reads give startup its pre-publication scan: `getAllNetworks`
  needs only `ACCESS_NETWORK_STATE` and returns every tracked network on Android 13-17, so a
  TestNetwork that outlived a previous app process is refused before this one registers an agent.

Nothing in this slice was device-verified when it landed: it compiled, the unit tests passed and the
Android native build was clean, but the handover, the reset, the abortive close, the SERVFAIL, the
report frame, the UI and the collision classifier had all been exercised only by review. The first device
pass has since covered the UI path - three row starts, the `Ready` -> `Active` -> `Ready` transitions and
the status-only row itself, one revision before the independence correction, which changed nothing about
that path; see [Device Qualification](#device-qualification-two-passes-one-revision-apart). The handover, the reset, the
abortive close, the SERVFAIL, the report frame and the collision classifier are still review-only, and
none of this closes a [Production Gate](#production-gates). In particular, telling a *truly foreign*
agent apart has not been qualified on a
device, and it is the one case a test living in this repository could not settle anyway: a second
agent published by *this* app proves the read path and says nothing about ownership, and only a
separately signed second app can produce a foreign one. So that gate is external by nature, and no
in-repo test is offered as evidence for it.

### Required Security And Lifecycle Tests

If the boundary is ever re-characterized - on another release, or against a
platform change - use a separately signed attacker APK with `INTERNET`,
`ACCESS_NETWORK_STATE`, and `CHANGE_NETWORK_STATE`, and assert the outcomes Step 2
actually established rather than the ones this document originally demanded:

- passive discovery may succeed;
- restricted active TEST requests must throw;
- unrestricted TEST requests must never match this restricted agent;
- Java, NDK, DNS, ping, process binding, and `SO_MARK` must put zero attacker
  packets on the TUN;
- interface binding and packet-info paths are **expected to deliver** packets, for
  the attacker and the owner UID alike. A run where they do not is the interesting
  result, and means the platform changed;
- the owner app UID must behave identically to the attacker on every path;
- a shell/system positive control must work;
- the old unrestricted TestNetwork agent must make the negative-control harness
  detect the original bypass. Published `setupTestNetwork` prototypes are that
  control in the wild: they are usable by any installed app by construction, and
  the harness is only trustworthy if it flags them.

Race every startup/cleanup boundary, including:

- app-process death before and after request/agent publication;
- Binder death/replacement before, during, and after wrapped calls;
- process death during TUN, request, and agent registration;
- Shizuku death while `ACTIVE` and during cleanup;
- cleanup-only Binder reauthorization with the same and a different effective
  UID;
- force-stop and uninstall;
- late callbacks after generation retirement;
- handle updates arriving back to back and while the previous sweep is still
  running, verifying that the latest value wins and that no work binds a swept
  generation;
- a handle update while TCP, UDP, and Echo traffic is actively sending, verifying
  that the cancel and join complete before the new generation is published and
  before any descriptor is closed, and that no send lands on the old generation
  after the swap;
- a handle update while a real DNS-over-TCP client is mid-exchange, verifying that
  its connection is *not* reset, that the answer resolved under the predecessor
  arrives as that query's own SERVFAIL, and that its next query on the same
  connection resolves on the new handle. The daemon's own tests cover this through
  its real owners; what they cannot cover is a client that is not this repository;
- sustained mapping churn, verifying that admission denies new mappings before the
  ephemeral range saturates rather than failing a bind;
- repeated handovers while the DNS pool is full, and a stop and restart with one
  outstanding, verifying that debt stays charged to completion and that the
  platform never answers a new resolution with `EBUSY` because the daemon
  over-admitted across the boundary;
- a handle update whose acknowledgement never arrives, one whose frame write fails
  mid-frame, and one the daemon reports as a failed sweep, verifying each ends the
  session explicitly, that the ordered stop fences the child so nothing continues
  on the superseded handle, that reapply recovers without root, and that no
  in-session relaunch or stream reuse is attempted;
- tethering moving off this TestNetwork while the agent stays up, verifying the
  downstream epoch advances, that its state is retired rather than merely closed
  to new admissions, and that admission reopens only after the daemon
  acknowledges the new epoch;
- killing the tethering service while the app survives, verifying the death
  recipient fires, that the session ends rather than continuing on a cached
  upstream, and that recovery is reported as needing an app restart because
  reapply would recollect against the dead connector;
- a stalled privileged request transaction, measuring how long process-wide
  connectivity callbacks are blocked behind `sCallbacks` and whether ordinary app
  components recover;
- IPv6 with two tethered downstreams, verifying which one receives the delegated
  `/64` and that the other degrades to IPv4 rather than failing, and with a
  local-only downstream started first, verifying whether any tethered interface
  gets IPv6 at all;
- a child that ignores SIGTERM, verifying the escalation reaches SIGKILL, that
  exit is observed, and that `STOPPING` does not strand the session;
- an old-epoch task enqueueing after the writer purge, verifying the join and the
  epoch gate drop it instead of sending it when admission reopens;
- an `ACTIVE` to `VERIFYING` to `ACTIVE` gap, and a Tethering service restart
  that leaves this session's `Network` handle unchanged, verifying both advance
  the epoch rather than resuming state whose NAT may have been rebuilt;
- stopping one of several downstreams, verifying what the global path claims: that the
  session state follows the upstream observation alone, and that a remaining client is
  carried or retired by that observation rather than by which interfaces are up;
- a reply arriving after its mapping retired, verifying the interface check drops it
  when the arrival interface is not the current generation's;
- two handle updates arriving during one sweep, verifying the superseded one
  completes as superseded instead of expiring at `control_result_deadline`;
- a handle update to no selectable `Network` and back, verifying that upstream
  work fails per operation, that the TestNetwork, agent, request, and downstream
  link stay up, and that the session resumes on the next handle;
- a handover onto a newly connected VPN while the old physical `Network` stays
  usable. Assert at the submission layer, where the property is decidable: no
  upstream send or resolver query is submitted on the old handle after the owner
  applies the new one, and TUN writes on swept state are limited to one terminal
  packet each. The capture on the old interface measures rather than asserts, and
  only over traffic this design owns. Filter it to the swept flows' and mappings'
  tuples and to the old `Network`'s DNS servers; the interface also carries the
  new VPN's own tunnel traffic, which by construction egresses there afterwards,
  plus unrelated app and system traffic. Within that filter, every packet the
  device transmits after the swap must fall in the classified set - queued
  segments already accepted by the kernel, kernel control traffic, and resolver
  transactions submitted before the swap - and the first and last of those do
  carry client payload, so record their volume and duration instead of claiming
  their absence. Peer-originated packets may keep arriving, payload
  retransmissions included; assert only that none of their contents reach the
  TUN.

On every supported Android release, exercise the wrapped `IConnectivityManager` by hand - with a
purpose-built harness rather than a test source set this repository maintains, since none exists -
and verify that:

- `sInstance` is identity-unchanged across construction, use, and cleanup;
- ordinary `Context` service lookup still returns its ordinary manager;
- `Network.openConnection()` and `setProcessDefaultNetwork()` proxy lookups do
  not reach the privileged wrapper;
- the private agent context is the only path that returns the privileged
  manager;
- the exact request registers with the effective-UID package, and the same
  request throws `SecurityException` when the app package is supplied on a
  shell-backed session;
- every privileged operation issues exactly one wrapped transaction, counted at
  the wrapper, so no operation can span two epochs.

Verify no leftover request, agent, native network, route, allowed-UID rule, or
TUN after every removable-state path. The accepted exception is the global
preference residue.

### Device Qualification: Two Passes, One Revision Apart

Two narrow passes ran on 2026-08-23, one revision apart, and which revision each ran against is the first
thing to read. A later start/stop smoke used the exact current tree, but did not repeat their traffic or
coexistence matrix. Read what they do *not* establish, at the end of this section, before treating any gate
as closed.

**Pass one: functional and dataplane, before the correction.** Everything from *What ran* down to *Cleanup*
below is that pass. It predates the root/Shizuku independence correction by one revision. It never ran root
routing, so none of its observations depended on the removed cross-mode ownership fence, and those
observations remain evidence for the paths the correction did not touch - publication, the state machine,
selection, delegation, the client traffic and the ordered stop. Two things it exercised are gone: the
cross-mode fence itself, and the per-downstream MTU floor it measured, which the corrected tree replaced
with the fixed `test_network_mtu` contract. Neither is claimed here.

**Pass two: independence and coexistence, on the corrected implementation.** *Post-correction independence
rerun* below is that pass. It installed the repaired tree on the same device, re-observed MTU 1500, and
exercised root/Shizuku independence directly rather than inferring it from the diff. The only change made
since it ran is the failure-reporting correction that routes an internal deadline through the operational
snackbar rather than past it; nothing that pass observed reaches that path, and nothing it observed
depended on it.

**What ran.** A rooted Android 17 / API 37 device with root-backed Shizuku. The debug APK (3.0.8) was
installed in place with `:mobile:installDebug`, app data
preserved. No instrumented test was built or run; this repository still has none.

**Startup, and the status-only row.** The `VPN tethering without root` row was started three times. With
system tethering off, each session reached `Ready` - the ARMED state - and nothing else on the page moved:
the repeater, the local-only hotspot, the watched-interface rows and the system-tethering controls were
untouched, and the row carried status and its own switch and nothing more.

**What the platform showed.** The three sessions each created a TestNetwork TUN at MTU 1500,
each carrying `192.0.2.1/30` and `2001:db8:1::1/64` with `192.0.2.5` and `fd00::53` as resolvers. The
published network was TEST and restricted, with neither `INTERNET` nor `NOT_RESTRICTED`, and carried an
exact `TestNetworkSpecifier` naming its own TUN with requestor package `be.mygod.vpnhotspot`. The exact
request was registered through the root-backed Shizuku service while each dataplane child ran under the app
UID, parented by the app process, and the pre-existing root control daemon stayed a
separate process throughout - the split this design rests on, observed rather than argued.

**Fail-closed while `Ready`.** With the second session armed and no tethering selected, a rooted
`ping -I <testtun> -c 3 -W 1 8.8.8.8` sent 3 and received 0, 100% loss, with interface counters moving TX by
exactly 3 and RX not at all. Packets entered the TUN and the daemon admitted none of them. That is the
negative check it looks like and nothing more - it is not evidence about forwarding or protocol correctness.

**Wi-Fi could not be the gate, for a reason outside this feature.** Two attempts to bring up the Wi-Fi
hotspot - one from the app's existing row, one through Android Settings - both ended in Android SoftAP state
`FAILED` with `failureReason 0` (general). Settings displayed `Error` on its own, no `wlan2` appeared, and
the app surfaced `other start failure` rather than `Permission missing`. That is the platform's SoftAP
failing on this device: not a selection failure, not an app regression, and no reason to touch tethering
code. The TestNetwork stayed `Ready` through both. The Wi-Fi hotspot path is therefore untested, not passed.

**USB tethering supplied the real `Active` gate.** Enabling USB tethering, with Wi-Fi untouched, created
`ncm0`; the row moved `Ready` -> `Active`; Android selected the session's TestNetwork TUN as the tethering
upstream and set its DNS forwarders to `192.0.2.5` and `fd00::53`. `ncm0` received `172.19.65.7/24` and
`2001:db8:1::7c/64`, and `2001:db8:1::/64` was delegated. The Windows UsbNcm host was a real external client
at `172.19.65.179` and `2001:db8:1:0:6883:27c0:d020:af3c`, and Android's BPF state showed that client, NAT
in both directions through that TUN, and live IPv4 and IPv6 forwarding.

**What that client actually did.** IPv4 ICMP to `8.8.8.8` was 3/3; IPv6 ICMP to `2001:4860:4860::8888` was
3/3; `nslookup example.com 172.19.65.7` returned A and AAAA through Android's tethering DNS proxy; and a
source-bound HTTPS request from `172.19.65.179` returned HTTP 204. A rooted direct-TUN probe while `Active`
returned 3/3 on IPv4 and **2/3** on IPv6, so nothing here claims zero-loss reliability.

**One live-traffic sample, not a bound.** Final daemon counters: UDP sent 454 / wrote 385; Echo sent 12 /
wrote 11; virtual DNS answered 216; TCP opened and closed 36 flows carrying 167,447 bytes upstream and
1,049,413 clientward; TUN output wrote 1,885 packets; peak records 66 and peak DNS tokens 5; zero admission
denials and zero invariant violations. Shutdown reached zero outstanding leases and the child exited 0. One
modest sample of a working session, measuring nothing about ceilings under pressure.

**Cleanup.** Normal row stop ran three times, each removing the app-UID child, the TUN, the restricted
network and its request, and `ShizukuTetheringService`, without clearing app data or killing the root
control daemon. The second and final stops each saw the app-UID child exit 0 and Connectivity release the
matching request and TUN. Both TUNs
were then absent and each session retired. Stopping USB tethering first returned `Active` -> `Ready` and
cleared the upstream with `ncm0` down; stopping the row afterwards left it unchecked, the app process and
root control daemon alive, the app-UID child, TUN and request gone, `ShizukuTetheringService`
absent, and system tethering reporting no wanted or current upstream.

**Post-correction independence rerun.** The repaired debug APK was installed in place on the same device.
Shizuku was first started while the pre-existing monitored root route for `wlan2` was already live. It
published a TEST network and request, with its app-UID child beside the separate root child. After Android
independently selected the TestNetwork TUN, root's unchanged priorities were
20700 to `tun0`, 20800 to `rmnet1` and 20900 unreachable, ahead of Android's priority-21000 rule to
the TestNetwork TUN; neither Shizuku service state nor any of its resource identities changed.

An external switch from the WAN adb transport to USB happened before the next snapshot. At that point
the TestNetwork TUN, its app-UID child and its service were already absent while the app process remained alive; the
relevant log window had rotated, so that transition has no assigned cause and is not counted as a passed
lifecycle gate. No replacement was launched until the exact service, TUN, child and TEST request were all
confirmed absent.

The controlled matrix then used USB tethering's `ncm0` and a fresh session with a new TestNetwork TUN,
request and app-UID child. With root routing off and Wi-Fi temporarily disabled,
the only `ncm0` forwarding rule at priorities 20700-21000 was Android's priority-21000 lookup of
the TestNetwork TUN; Android reported that TUN as the current upstream and non-zero forwarded statistics. A Windows
client at `172.19.65.179` sent twenty 1,200-byte pings to `1.1.1.1`, all of which returned. Across that probe
the TUN moved by 26,806 RX bytes / 38 packets and 27,754 TX bytes / 49 packets.

Starting ordinary root routing for `ncm0` added priorities 20700, 20800 and 20900 ahead of the unchanged
priority-21000 TestNetwork rule. The TUN, network, request, child and the
Shizuku service's original start identity all remained unchanged. The same twenty-packet client probe was
20/20 while the TestNetwork TUN moved by only 1,686 RX and 2,302 TX bytes - nine background packets each way - and
`tun0` moved by 191,380 RX and 102,193 TX bytes. The `tun0` totals include unrelated phone traffic, so they
are corroboration rather than attribution; the rule ordering, source-bound successful probe and absence of
the probe-sized TUN delta are the root-override evidence.

Stopping only root routing removed priorities 20700-20900 and its root child while every Shizuku identity
above remained unchanged. Repeating the same twenty-packet probe was again 20/20 and moved the TestNetwork TUN by
the probe-sized deltas quoted above. Starting root once more and then stopping only Shizuku removed the
TestNetwork TUN, network, request, child and `ShizukuTetheringService`, while the root child and
priorities 20700-20900 remained; a final five-packet client probe was 5/5. This is the device evidence for
the actual design: neither mode coordinates with the other, and existing root policy wins automatically.

Cleanup removed the temporary `ncm0` root route, restored the original inactive monitors
`ap_br_wlan2` and `wlan2`, re-enabled phone Wi-Fi, and removed the temporary Quick Settings tile and UI
hierarchy file. Normal USB tethering remained active with `wlan0` as its ordinary upstream. No TestNetwork,
Shizuku service or app-UID child remained, and no instrumented test was built or run.

**Exact-current-tree lifecycle smoke.** After the deadline failure-reporting correction, the final debug APK
was built with `:mobile:assembleDebug`, installed in place, and started as the app UID on the same device. It
published a TestNetwork TUN at MTU 1500, together with its TEST network, request and app-UID child. The daemon
reported ready, selected the existing VPN `tun0` as
its global egress, and logged no startup exception or deadline. A normal stop then closed admission, the
child exited 0, the request was released, and the service, TUN and child were all absent while the
pre-existing root control child remained. This is exact-tree evidence only for startup, publication and
ordered teardown; it does not repeat or enlarge either substantive matrix above.

**Separate non-rooted shell-Shizuku dataplane pass.** The exact current debug APK was then installed on a
different stock, non-rooted Android 17 device. Shizuku itself ran as the shell UID. Granting the app's
ordinary Shizuku request created a TestNetwork TUN at MTU 1500, with a TEST network and request attributed
to package `com.android.shell`; the dataplane child ran separately as the app UID. No root child existed and
policy priorities 20700-20900 were
absent.

Phone Wi-Fi was temporarily disabled only to remove the competing ordinary upstream, then USB tethering
was enabled through Android Settings rather than through this app. Android tethered `ncm0`, selected the
TestNetwork TUN through its priority-21000 rule, delegated `2001:db8:1::/64`, and exposed
`10.150.234.31/24` to a Windows client at `10.150.234.108` with IPv6
`2001:db8:1:0:4236:75f0:53aa:1ef5`. Tethering's own state named the TestNetwork TUN as the current upstream
and its BPF maps showed IPv4 NAT and IPv6 forwarding between `ncm0` and that TUN; the app's row showed only
`Active`.

From that client, twenty 1,200-byte IPv4 pings to `8.8.8.8` returned 20/20 and ten 1,200-byte IPv6 pings to
`2606:4700:4700::1111` returned 10/10. `nslookup example.com 10.150.234.31` returned A and AAAA records, and
source-bound IPv4 and IPv6 HTTPS requests both returned HTTP 204. Two earlier ICMP samples were deliberately
not promoted into a reliability claim: `1.1.1.1` returned 8/20 over IPv4, and
`2001:4860:4860::8888` returned 5/10 over IPv6 while a direct-phone control returned 9/10. The second IPv6
destination returned 10/10 both directly and through the client. At stop the daemon therefore reported 60
echo requests sent, 43 written and 17 swept, with no send failure, unmatched reply or invariant violation;
the clean destination-matched probes, DNS and both HTTPS families establish function, not zero-loss quality.

The stop snapshot also reported 27,374 UDP sends, 29,630 UDP writes, 406 virtual-DNS answers, and 82 TCP
flows opened and closed carrying 735,865 bytes upstream and 3,314,365 bytes clientward. Peak admission was
163 records and seven DNS tokens, with no denial or invariant violation. USB tethering was turned back off,
Wi-Fi was restored to its original enabled state, the app-UID child exited 0, the request was released, and
the service and TUN were absent while the external shell Shizuku server remained. The debug APK and
its Shizuku grant were intentionally left installed. No instrumentation was built or run.

**What these passes do not establish.** Two devices on one Android version, one debug app build, one
client and one transport for the external packet oracle. They say nothing about fragmentation, path-MTU signalling, TCP edge cases or DNS
failure modes; Shizuku VPN egress or
handover; isolation and the security posture; resource pressure or any measured ceiling; disruptive failure
injection - Shizuku death, a UID change, a child that never authenticates, the TERM/KILL fence; a release
build; Android 13-16, OEM or Mainline variants; a foreign TestNetwork; or the Wi-Fi hotspot path. Every gate
in [Production Gates](#production-gates) naming one of those is still open.

The host checks that ran for the pass that landed the Android side, all clean:

```
git diff --check                          # run on its own, so neither result masks the other
git diff --cached --check
rustc --version --verbose                 # 1.98.0 (88d9e12ae 2026-08-18), the host's own stable
cd mobile/src/main/rust/vpnhotspotd
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo test --locked                       # 354 library + 132 binary tests, host only
cargo clippy --locked --all-targets -- -D warnings
cd -
./gradlew --offline :mobile:buildDebugDaemonNativeLibs :mobile:compileDebugKotlin \
          :mobile:testDebugUnitTest
                                          # native libs for all four ABIs; 89 JVM unit tests, including
                                          # ShizukuLifecycleTest, SessionPublicationTest,
                                          # PreferenceResourceTest, BinderPublisherTest,
                                          # AppDefaultStateTest
```

Those counts are a result rather than an expectation: they are what the runs above printed. They were
re-run after the root/Shizuku independence correction, which removed the acknowledged-teardown fence and
its six daemon tests along with it; the root IPsec probe owner and generation tests are among what remains.
The later exact-tree lifecycle smoke also ran `assembleDebug`; it is reported separately above rather than
included in these test counts. `cargo test --locked` is what CI runs too, against the same
`Cargo.lock`, so a dependency the accounting does reason about - the channel figures - is executed there
rather than only compiled. Nothing here is pinned to a compiler version: the accounting no longer makes
any claim that a compiler could invalidate - see [Resource Policy](#resource-policy).

Everything below the pass above did not touch, and none of it is claimed as evidence for the current
architecture:

- default/VPN/excluded-app handover;
- repeated hotspot cycles reaching `ACTIVE`, and any Wi-Fi-borne one. The pass above reached `ACTIVE` once,
  over USB, with no root routing live; the independence rerun reached it again and exercised a monitored
  root route on the USB downstream, but this device's earlier SoftAP gate failed on its own and no controlled
  Wi-Fi client oracle was run, so the Wi-Fi dataplane is untested;
- a truly foreign TestNetwork, which needs a second controller and cannot be synthesized by this app;
- exact cleanup under Shizuku death and under a Shizuku identity UID change;
- running this mode beside the repeater or local-only hotspot. Monitored-interface coexistence and automatic
  root precedence now have the one-device evidence above; those separate root-mode owners do not;
- the child TERM/KILL fence, including a child that never authenticates;
- fragment, extension-header, path-MTU, TCP edge-case and DNS failure-mode dataplane behaviour, the outer
  TCP idle floors among it - [Outstanding Daemon Work](#outstanding-daemon-work) no longer blocks this, and
  the pass above exercised only ordinary IPv4/IPv6 ICMP, DNS-proxy and HTTPS/TCP traffic for one client;
- resource pressure and every measured ceiling;

A usable VPN topology and a second foreign TestNetwork controller are prerequisites for parts of that list
even now that a client has been attached once.

### Outstanding Daemon Work

The Android side of this mode is finished, and the Rust dataplane's own outstanding list is now empty.
That is not the same as ready. Two narrow same-device passes exist, one revision apart - see
[Device Qualification](#device-qualification-two-passes-one-revision-apart) - the earlier one covering
publication, the fail-closed `Ready` state, `Active` selection over USB, one client's ordinary traffic and
normal cleanup, and the later one, on the corrected implementation, monitored-interface coexistence with
root. Everything else that has landed is proved by the
daemon's own tests through its real owners and by nothing else, and every gate in
[Production Gates](#production-gates) those passes did not reach is still open.

**Outer TCP state lifetimes have landed**, which closes the last item this section used to hold. The floors
are RFC 5382 REQ-5's, classified by the actual post-poll `smoltcp` state and described under
[Timers](#timers): one optional monotonic deadline in the flow record the engine already keeps, a bounded
linear minimum folded into the deadline the ingress task already sleeps on, and expiry applied on that same
wake, judged against the instant captured before that poll - which defers by one loop anything that came
due while the stack was running, in the conservative direction. There is no timer task, no queue, no second table and no
new configuration surface; the deadline is inside the flow's existing footprint, so it is charged by the same
reservation the flow already took. A cancelled flow is excluded from the minimum, because cancelling does not
remove one: what removes it is whichever of its two endings applies - an attached flow leaves when its worker
finishes, and a detached one has no worker left and leaves when the ingress owner's own scan finds its client
finished. A passed deadline left in the schedule would spin that loop until whichever ending was due arrived.

One simplification went with it, and it is worth being exact about what it did *not* fix. The
finished-socket scan used to treat a `Closed` socket as terminal only once the flow had been established,
and that gate is gone: a socket the stack has finished with is finished. It strands nothing today, because
a client cannot drive a flow of this engine's to `Closed` before it is established at all - every flow is
passively opened, and smoltcp ignores a reset in `LISTEN` outright and returns a `SYN-RECEIVED` socket to
`LISTEN` rather than closing it (smoltcp-0.13.1, `src/socket/tcp.rs:1818-1831`). Every non-established
`Closed` therefore comes from this daemon's own `abort`, which cancels or removes the flow in the same
breath. What actually ends a handshake the client walked away from is the transitory floor, and it ends it
silently: a listening socket has no remote endpoint for the stack to answer with a reset.

What is *not* claimed is post-RST retention; see [Timers](#timers) for why, and note that host tests against
an injected clock are not device evidence for any of it.

**The IPv4 Identification nonreuse window has landed**, and what it closes is a receiver-facing
invariant rather than a leak: the same `(source, destination, protocol, Identification)` must not reach
the wire twice inside the sixty seconds a downstream may hold fragments. What that took is described in
[The Nonreuse Window](#the-nonreuse-window), and three of its parts replaced something that did not add
up. A per-tuple sequence now *ends* at 65,536 rather than wrapping, so the datagram after a spent cycle
is denied instead of being handed a value its predecessor may still be carrying. The window is measured
from the TUN writer's own successful write rather than from the moment a value was issued, because a
packet accepted into the queue may be parked waiting for the kernel indefinitely and an allocation
timestamp is therefore not a wire timestamp - so while the session continues every guarded packet ends
in exactly one settlement back to the ingress task, and a stale dequeue, a validation refusal and a
retirement-preempted blocked write are all endings that were never written and start no window. The
session's own endings settle nothing, and the successor's opening window is what covers them. And every new session denies guarded output
for its first sixty seconds, which is the only thing that covers what a predecessor wrote a second
before it stopped. A full table now reclaims the slots of occupants that can no longer collide - the same
test a restart has to pass - at most once per sixty seconds, so it stops being a table that fills once
and denies for the rest of the session without ever evicting anything live.

Two things about how it is owned are worth naming. The allocator stays the ingress task's alone: there
is no lock, no per-flow allocator, no task per tuple and no timer, and the whole decision is one hash
lookup on the datagram's own path. And the settlement channel is sized so it cannot fill - one slot for
every guarded packet the writer can be holding at once, with the allocator refusing to register beyond
that - because a writer that had to wait to report an ending could not reach the retirement it is being
asked to acknowledge.

What has *not* happened is any of it on a device. The MTU-1500 gate in
[Production Gates](#production-gates) still names Identification guards, and it is still open.

What *has* landed on the Rust side and should not be regressed: the TUN writer's retirement command and
acknowledgement with blocked-write preemption, the dequeue stamp gate, UDP reserve-before-send rollback,
UDP DNS generation and epoch debt gating, cancellation-aware TCP connect, and the improved reassembly
footprint and 32-bit memory parsing.

One of those deliberately did **not** touch root, and one deliberately does. The control writer is now a
single cancellation-aware implementation shared by both conversations: a write failure ends the conversation
rather than being logged and survived, because the root read loop parks on `recv_packet` and a peer that
closed only its read half would otherwise leave it there with live routing and live IPsec probes behind it.
The reporter split is the one that stayed apart: root keeps its
process-global channel flushed once at the end of the conversation, while an app session owns a reporter
whose finish is part of the session's own result. Folding those together would have changed root's behaviour
to suit a mode it does not run.

**DNS-over-TCP generation semantics have landed**, which closes the last of the dataplane's DNS work. A
flow records what it *is* when it is opened, so retirement splits by axis: an epoch change retires
everything, a generation change retires exactly the flows holding a selected-network socket, and a
virtual-DNS transport - which holds none - keeps its socket, its mailbox, its logical token and its
outstanding question across the handover, joined and refunded flows aside. It needs no selected network to
*open*, either, for the same reason it survives one changing: a connection accepted while the session has
none answers its questions with their own SERVFAIL and resolves normally once a config supplies one.
Neither axis cancels or awaits a resolver transaction, so a config acknowledgement never waits on a name
server. Each query is stamped and given its `Network` where the serialized ingress owner *accepts that
query*, so one queued before a config and accepted after it belongs to the successor, and an answer is
stale by generation even when the successor kept the same handle. Settlement classifies on the retained
stamp and the exact `(handle, worker)` pair: a matching stamp delivers the answer; the same epoch with an
older generation drops the obsolete result, builds that query's own SERVFAIL from the query the
transaction kept, parks the delivery and hands it to a stream that stays usable; an epoch mismatch, a
replaced flow, a reused handle or a wrong worker is silent and refunds exactly once. A query is admitted
at the length its prefix announces before a byte of it is stored, with a second tier that answers rather
than drops when the descriptor floor is full, and no selected network or an expected resolver outcome is
likewise one SERVFAIL on a connection that carries on.

Three things about *how* it is owned are worth naming. The transactions are a prepared, charged table the
ingress owner polls rather than a task per query - independent of the transport's lifetime, which is the
invariant, without the spawn, the token and the three oneshots that went with it. One of those oneshots was
built before its query had any grant, which was a real ordering fault; the task cell and the token were not,
because those are count-bounded rather than byte-charged under
[What Is Byte-Charged And What Is Count-Bounded](#what-is-byte-charged-and-what-is-count-bounded), so the
table's value is the query lifetime and the ordering rather than recovered bytes. A per-flow footprint
charges the two stack buffers, the three
payload chunks that can really be alive at once and all five of the flow's bounded channels, taken before
any of them is built, in place of a fourth chunk that was standing in for uncounted heap. And a submission
the platform accepted but this process can no longer observe - whether the watch was never possible or was
lost afterwards - quarantines its logical token for the rest of the session instead of refunding it, since
refunding a slot Android still holds is what drives its per-UID limiter into `EBUSY`. That last outcome is
terminal for the stream whatever the generation says, because a transport whose token has just been
quarantined cannot ask again.

What has *not* happened is any of it on a device: this is covered by the daemon's own tests through its
real owners, and by nothing else. The device run recorded under [Step 8 Status](#step-8-status) predates
this implementation and does not qualify it.

**Aggregate admission has landed**, and it is the only accounting path: one owner in the ingress task
holds two non-fungible totals - record/descriptor units and Rust-visible owned heap bytes - and every
mapping, remote, flow, Echo socket, Echo session, resolver transaction, table, queue and scratch buffer
is a lease against it. The fragment cap and the resolver's logical-token cap are nested *checks* inside
those totals rather than pools beside them. The per-transport counters this replaces are gone rather
than left running in parallel: there is no second reserve, refund, flow or query path anywhere in the
daemon. What that means concretely is described in [Resource Policy](#resource-policy) - two totals, a
floor that is inside its total rather than subtracted from it, leases that never refund themselves,
prepared collections that refuse rather than grow, each on one explicit logical maximum row count that is
also what its charge was computed from - a removal frees a slot the next arrival takes, and what a hash
container does with its own backing is opaque count-bounded overhead outside byte attribution - and growth
of a bound charged as an old-beside-new replacement.

**Per-flow TCP output has landed** too. The global pending chunk and the `accepting()` gate in front of
the flow-event channel are gone; each flow has a depth-one mailbox at the read quantum, an explicit
consumption acknowledgment its producer waits on before reading again, and owner-confined pending state
carrying the exact offset. A DNS-over-TCP transport that half-closes on a clean message boundary enqueues an
ordered end of stream and waits for it to be consumed before returning, so the lifecycle terminal cannot
overtake the answer it follows; one that half-closes mid-message has truncated its own request and is reset
instead. What circulates between flows is a payload-free readiness marker, deduplicated
per identity, serviced in an explicit round-robin whose budget is taken when the round begins - in *both*
directions, because a `HashMap` iteration is not fairness but a different unfairness each time. Every
signal carries `(SocketHandle, worker id)` and the pair is validated before any close, reset, report or
socket side effect, because smoltcp reuses handles and a stale terminal must not reset the successor that
took one. Retirement discards that exact identity's payload, ordered EOF and queued marker *before* it
cancels the worker, because a cancellation that bypassed an acknowledgment wait before the owner
committed to dropping what the wait was for is a worker released while its owner still believes it owes
bytes.

Task and descriptor ownership has landed too, and it is the fence the rest of this list stands on, so it
deserves naming precisely. Every descriptor-bearing worker - UDP mapping receives, ping-socket receives,
TCP flow upstreams, DNS-over-TCP transports, UDP resolver transactions, and the reporter's window task -
has a retained task handle in `workers.rs` or, for the reporter, in `shared/reporter.rs`. No terminal
message exists any more: a worker's *completion* is the terminal event, and its owner joins it before it may
take the record back, refund, and let the acknowledgement go. For a terminating TCP flow the two are not the
same moment: a clean completion whose client is still closing detaches, and the record and its charge go when
that teardown finishes - see [A Flow Can Outlive Its Worker](#a-flow-can-outlive-its-worker). A DNS-over-TCP resolver transaction is
the one owned thing here that is not a task: it is a row in a prepared table the ingress owner polls, and
dropping that row is what returns its descriptor - the same fence without the spawn. Every wait inside a worker races its own
token through `shared/preempt.rs`, so a stalled peer, a backpressured write or a full event queue cannot
delay a retirement. Whole-session EOF, cancellation and failure all run the same quiesce-and-join over
every owner before the ingress task returns. What that fence does *not* cover is the platform's resolver
work: dropping a query returns this process's descriptor and nothing of Android's, and neither retirement
nor process death settles the platform's own accounting.

### Production Gates

Do not claim production support until all of these are resolved:

- release-APK app-UID native launch and SELinux/`SCM_RIGHTS` behavior;
- ordinary in-process Shizuku provider delivery and wrapped PFD return on every
  supported Android 13-17/Mainline variant;
- shell/root AppOps attribution plus `MANAGE_TEST_NETWORKS`,
  `CONNECTIVITY_USE_RESTRICTED_NETWORKS`, and `NETWORK_SETTINGS` permissions;
- exact `ConnectivityManager` field access, constructor-less construction,
  unchanged `sInstance`, and ordinary proxy behavior across supported
  framework-module variants;
- restricted netd enforcement of `Network`-handle selection against same-app and
  cross-app UIDs, on releases other than Android 17. Interface-level injection is
  not a gate, because it is known to be permitted;
- exact request/agent/tethering selection and cleanup on supported OEMs;
- selected VPN support for TCP, unconnected UDP source pinning, ping sockets,
  hop controls, DF modes, and required error-queue metadata;
- abortive TCP retirement and measured post-swap residue on an old `Network`, its
  queued-segment, private-DNS, plaintext, and DNS-over-TCP components separated,
  per supported release;
- the daemon's DNS ceiling measured against the platform's per-UID limiter,
  proving repeated handovers cannot drive a cancelled-query backlog into `EBUSY`;
- actual Android inner-NAT and DNS-proxy packet shapes;
- a globally preferred tethering prefix. A non-forgeable IPv6 client-ownership
  mechanism was a gate in earlier drafts and has been struck: no such mechanism
  exists at this privilege level, which is why both families collapse to shared
  principals;
- the user-facing statement of [Security Posture](#security-posture) in `README.md`,
  since a rootless mode presented as equivalent to root mode would misrepresent what
  it protects. Deliberately *not* in the mode's settings row: that row carries the
  current state and nothing else, like every other row on that page, and a paragraph
  of posture under a switch is not where this belongs;
- MTU-1500 downstream behavior including path-MTU signalling toward clients when
  the selected network is smaller, IPv4 fragmentation, IPv6 source fragmentation,
  Identification guards, and partial-write cleanup;
- measured descriptor/memory ceilings and reserves, the DNS query limit, fragment
  headroom, and the UDP error-history bound **on a device**. The derivation and the enforcement
  have landed and are covered by platform-neutral tests; what has not happened is watching the
  numbers a real device produces under real traffic, which is the only way the prepared capacities
  and the reserved floor can be shown to be sized right rather than merely consistent.
  Per-principal targets are struck: they were removed from
  [Resource Policy](#resource-policy) along with the dynamic principal set they
  existed for, so there is nothing left to measure;
- packet semantics and cleanup across Android 13-17 and supported OEM/Mainline
  variants.

Pixel/Android 17 evidence exists for the real Shizuku wrapper, constructor-less
manager construction with an unchanged singleton, effective-UID attribution on the
root branch, the restricted agent and exact request, complete cleanup, a separately
signed attacker, and - from [Step 3](#step-3-verdict-passed) - one selection of a
restricted TestNetwork as a tethering upstream with its `/64` delegated and MTU 1500
propagated. What it settles negatively is the isolation premise: see
[Step 2 Verdict: Failed](#step-2-verdict-failed).

Two limits on that historical evidence, and they are why it closes no gate. It is
**platform** evidence: each run answered a question about what Android does, on the
implementation of its own day, and the dataplane, the resource model and the session
lifecycle have been rewritten since. And even as platform evidence it is narrow:
selection was observed twice and [found racy](#selection-is-racy) afterwards, no run
involved a dataplane at all (packets reaching the TUN were dropped, because nothing was
reading it), and Android 13-16, OEM and Mainline variants and the shell attribution
branch were never exercised.

The two recent passes are recorded separately, under
[Device Qualification](#device-qualification-two-passes-one-revision-apart). They close none of
the gates above either, and are deliberately not merged into the list: between them they cover
publication, the fail-closed `Ready` state, `Active` selection with a real external
client over USB tethering, that client's ordinary IPv4/IPv6 ICMP, DNS-proxy and
HTTPS/TCP traffic, and normal cleanup - one revision before the independence correction - and
monitored-interface coexistence with root on the corrected implementation, all on one
rooted Android 17 device, debug build. Release-build behaviour,
Android 13-16 and OEM/Mainline variants, Shizuku VPN egress and handover, isolation,
broader packet semantics, measured ceilings, disruptive failure injection and the
Wi-Fi hotspot dataplane remain untouched, so every gate above stays open.

Residual `preferTestNetworks=true` after abnormal termination is an accepted
constraint, conditional on exclusive TestNetwork API use and on the root `README.md`
saying plainly what clears it - the mode's own next start and stop, or a reboot -
since there is no recovery surface to point at.

## Documentation At Adoption

The implementation has landed and is covered by the host record in
[Implementation Status](#implementation-status); what is still open is device qualification, per
[Production Gates](#production-gates). So the list below is no longer "when the code exists" - it is what
adoption still owes the reader, and the descriptor inventory in particular is a re-verification of what the
code now really touches rather than a plan for what it might:

- inventory every exact hidden/private descriptor and jarjar assumption in the
  appropriate root or hidden-stub README;
- include the exact members this design touches, each checked against
  `../hiddenapi/hiddenapi-flags.csv`, which on Android 13-17 are the assigned
  fields, the read-only `sInstance` invariant, every field reached by the copy,
  the feature-cache warmer, the allocator, and the preference call:
  - `Landroid/net/ConnectivityManager;->mContext:Landroid/content/Context;,max-target-o`
  - `Landroid/net/ConnectivityManager;->mService:Landroid/net/IConnectivityManager;,max-target-p`
  - `Landroid/net/ConnectivityManager;->sInstance:Landroid/net/ConnectivityManager;,max-target-o`
  - `Landroid/net/ConnectivityManager;->mNetworkActivityListeners:Landroid/util/ArrayMap;,max-target-o`
  - `Landroid/net/ConnectivityManager;->mTetheringEventCallbacks:Landroid/util/ArrayMap;,blocked`
  - `Landroid/net/ConnectivityManager;->mTetheringManager:Landroid/net/TetheringManager;,blocked`
  - `Landroid/net/ConnectivityManager;->mQosCallbackConnections:Ljava/util/List;,blocked`
  - `Landroid/net/ConnectivityManager;->mEnabledConnectivityManagerFeatures:Ljava/lang/Long;,blocked`
  - `Landroid/net/ConnectivityManager;->mEnabledConnectivityManagerFeaturesLock:Ljava/lang/Object;,blocked`
  - `Landroid/net/ConnectivityManager;->isFeatureEnabled(J)Z,blocked`
  - `Landroid/net/IConnectivityManager;->startOrGetTestNetworkService()Landroid/os/IBinder;,blocked`
  - `Landroid/net/IConnectivityManager$Stub;->asInterface(Landroid/os/IBinder;)Landroid/net/IConnectivityManager;,unsupported`,
    unavoidable because `mService` is typed `IConnectivityManager` while the pinned wrapped value is
    an `IBinder`. Unlike the TestNetwork interface this type is not relocated, because the module's
    jarjar generator excludes everything in its UnsupportedAppUsage inventory and
    `IConnectivityManager$Stub$Proxy` members are listed there on every supported release
  - `Landroid/net/NetworkCapabilities;->TRANSPORT_TEST:I,blocked`
  - `Landroid/net/ConnectivityManager;->TYPE_TEST:I,blocked`
  - `Landroid/net/connectivity/android/net/ITestNetworkManager$Stub;->asInterface(Landroid/os/IBinder;)Landroid/net/connectivity/android/net/ITestNetworkManager;,blocked`
  - `Landroid/net/TestNetworkManager;-><init>(Landroid/net/connectivity/android/net/ITestNetworkManager;)V,blocked`
  - the same two members under the unrelocated `Landroid/net/ITestNetworkManager`
    spelling used on Android 13, recorded as absent from
    `../hiddenapi/hiddenapi-flags.csv`, which was taken from a build where the
    type is relocated. Document the absence; do not synthesize a flag for them
  - `Landroid/net/TestNetworkManager;->createTunInterface([Landroid/net/LinkAddress;)Landroid/net/TestNetworkInterface;,blocked`
  - `Landroid/net/TestNetworkInterface;->getFileDescriptor()Landroid/os/ParcelFileDescriptor;,blocked`
  - `Landroid/net/TestNetworkInterface;->getInterfaceName()Ljava/lang/String;,blocked`
  - `Lsun/misc/Unsafe;->theUnsafe:Lsun/misc/Unsafe;,unsupported` and
    `Lsun/misc/Unsafe;->allocateInstance(Ljava/lang/Class;)Ljava/lang/Object;,unsupported`. These are
    greylisted, so they belong in this bucket rather than with the agent surface below

  Two blocked members this design named turn out to be unnecessary and are not used:
  `Landroid/net/TestNetworkSpecifier;-><init>(Ljava/lang/String;)V,blocked`, because
  `Landroid/net/NetworkRequest;->getNetworkSpecifier()Landroid/net/NetworkSpecifier;` is `public-api`
  and the request's own specifier is reused; and
  `Landroid/net/NetworkCapabilities$Builder;->setAllowedUids(Ljava/util/Set;)Landroid/net/NetworkCapabilities$Builder;,blocked`,
  because a fresh builder already submits an empty set.

  The agent surface is `sdk,test-api` rather than blocked, so the runtime imposes no
  hidden-API restriction on it and it belongs in
  `mobile/src/hiddenApiStubs/README.md` or behind a compile-only stub, not in the
  private-API list. That placement is about runtime reachability and not about public
  API status: `sdk` is also what a member with no `hiddenapi` line is recorded as, and
  the CSV synthesizes no source-list metadata, so a missing `system-api` token proves
  nothing either way. `NetworkAgent` is in fact `@hide @SystemApi`
  ([source](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java#94)) -
  absent from that module's `framework/api/current.txt`, present in its
  `framework/api/system-current.txt` - while its rows still read
  `Landroid/net/NetworkAgent;->register()Landroid/net/Network;,sdk,test-api` and
  `Landroid/net/NetworkAgent;->markConnected()V,sdk,test-api`:

  - `Landroid/net/NetworkAgent;-><init>(Landroid/content/Context;Landroid/os/Looper;Ljava/lang/String;Landroid/net/NetworkCapabilities;Landroid/net/LinkProperties;Landroid/net/NetworkScore;Landroid/net/NetworkAgentConfig;Landroid/net/NetworkProvider;)V,sdk,test-api`,
    the `NetworkScore` overload; the `LocalNetworkConfig` overloads are `blocked`
    and are not used
  - `Landroid/net/NetworkAgent;->register()Landroid/net/Network;,sdk,test-api`
  - `Landroid/net/NetworkAgent;->markConnected()V,sdk,test-api`
  - `Landroid/net/NetworkAgent;->unregister()V,sdk,test-api`
  - `Landroid/net/NetworkAgent;->onNetworkUnwanted()V,sdk,test-api`, the terminal agent-channel
    callback retirement requires, delivered from `NetworkAgentInfo.disconnect()` whether or not a
    native network was ever created
  - `Landroid/net/NetworkAgent;->onNetworkCreated()V,sdk,test-api` and
    `Landroid/net/NetworkAgent;->onNetworkDestroyed()V,sdk,test-api`, the publication and destruction
    callbacks the barrier awaits
  - `Landroid/net/NetworkAgent;->getNetwork()Landroid/net/Network;,sdk,test-api`, read back to
    classify a registration that threw
  - the `NetworkScore$Builder`, `NetworkAgentConfig$Builder` and `NetworkCapabilities$Builder` members
    that build the immutable initial state, plus
    `Landroid/net/NetworkCapabilities;->NET_CAPABILITY_NOT_VCN_MANAGED:I,sdk,test-api`
  - `Landroid/net/LinkAddress;-><init>(Ljava/net/InetAddress;I)V,sdk,test-api` and
    `Landroid/net/RouteInfo;-><init>(Landroid/net/IpPrefix;Ljava/net/InetAddress;Ljava/lang/String;I)V,sdk,test-api`.
    Both owner classes are already in `android.jar` without usable constructors, so they are reflected
    at the call site rather than stubbed; the three-argument `RouteInfo` overload AOSP's own test
    networks use is `max-target-r` and unusable here. The rest of the `LinkProperties` surface this
    design needs is `public-api`
  - `Landroid/net/ITetheringConnector;->setPreferTestNetworks(ZLandroid/net/IIntResultListener;)V,blocked`.
    This one is done rather than owed: the preference step has landed, and the member is listed in the
    root `README.md`'s used-private-API section with the reason no less restricted alternative works -
    the `TetheringManager` shim is `blocked` too, discards the result code that is the only report of a
    missing `NETWORK_SETTINGS` grant, and blocks its caller for a minute
- re-verify every descriptor against `../hiddenapi/hiddenapi-flags.csv` at
  adoption, and record for each `blocked` member why the less restricted
  alternative is insufficient;
- finish moving this handoff from proposed to actual behaviour. The sections the slices have reached
  describe what the code does and say so; what is left is the prose no status section claims, which is still
  design;
- **Done.** `RESTART_REQUIRED` is surfaced in the row as terse status - "System tethering is not
  using this connection" - rather than as an error; what to do about it belongs in `README.md`,
  because the row carries status like every other row on that page;
- **Done.** Upstream fallback is explained in `README.md`: losing the session returns the hotspot
  to an ordinary upstream rather than closing it, and nothing on the client side says so;
- **Done.** [Security Posture](#security-posture) is stated in `README.md` in user-facing terms -
  what another app on the device can do while a session runs, what it cannot, and that root mode
  is unaffected - rather than as a general disclaimer;
- **Done.** `lifecycle.md`, `invariants.md`, `errors.md` and `traffic.md` are updated, including
  `traffic.md`'s accounting caveat: in this mode upstream bytes cannot be attributed to the client
  that really sent them;
- **Done.** `routing.md` carries the TUN addresses, routes, native-network permission, empty
  allowed-UID state and every normal/Binder/process-death cleanup path, together with the two
  residues that outlive a session;
- keep `daemon.proto` as the canonical IPC contract. The downstream epoch, the
  admission state, and the applied-generation acknowledgement go on the Shizuku
  daemon's own messages rather than on `SessionConfig`; see
  [Control Surface Decision](#control-surface-decision).
