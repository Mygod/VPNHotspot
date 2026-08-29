# Shizuku Mode

Shizuku mode shares the app UID's default `Network` with tethered clients
without root. It publishes a restricted `TRANSPORT_TEST` network over an
app-owned TUN, asks Android tethering to prefer it, and relays traffic from an
app-UID child process. It requires Android 13 (API 33) or later.

This is one global upstream mode. It neither owns nor starts downstream
tethering. Shizuku privilege is used only for Android control operations; the
dataplane and all egress sockets run at the app UID. Root and Shizuku modes have
independent state and may run together.

## Limits

- Other apps are not isolated from the TUN interface. See
  [Security Boundary](#security-boundary).
- Android NAT removes physical-client identity before traffic reaches the TUN.
  Per-client blocking and accounting are unavailable.
- The dataplane carries TCP, UDP, virtual DNS, ICMP Echo and supported ICMP
  errors. Other IP protocols and downstream link control are dropped.
- Android delegates the upstream IPv6 `/64` only to its oldest active
  downstream ([AOSP](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#201)).
  Other downstreams are IPv4-only; an older local-only downstream can prevent
  all tethered downstreams from receiving IPv6.
- `setPreferTestNetworks` sets a preference; it does not force tethering to
  reselect its current upstream.
- Losing the TestNetwork does not stop tethering. Android selects an ordinary
  upstream and clients continue unprotected without a client-side warning.
- Startup refuses any existing `TRANSPORT_TEST` network. A foreign test network
  selected as the tethering upstream ends the session.

## Ownership And Lifecycle

| Owner | State |
| --- | --- |
| [`ShizukuTestNetwork`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/ShizukuTestNetwork.kt) | TUN, agent, exact request, tethering connector and preference, child, upstream observation and cleanup ledger |
| [`ShizukuLifecycle`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/ShizukuLifecycle.kt) | current job and process-wide predecessor cleanup |
| [`ShizukuTetheringService`](../../mobile/src/main/java/be/mygod/vpnhotspot/ShizukuTetheringService.kt) | foreground scope and the session job |
| [`AppUidDaemon`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/AppUidDaemon.kt) | child process, peer authentication and control conversation |
| Daemon TUN ingress | TUN-visible flows, mappings, transports and their workers |

The foreground service owns one cancellable session job. Start and stop return
immediately; a successor joins its predecessor before authorizing or acquiring
anything. The job's `finally` is the only teardown, runs non-cancellably, and
withdraws only its own published session. Unconfirmed cleanup remains recorded
and is retried by the next command. Service recreation retains the process-wide
predecessor fence.

The app-UID daemon is an ordinary child, not a Shizuku process. Control EOF makes
a healthy child exit; ordered stop additionally waits 10 seconds, sends SIGTERM,
waits 5 seconds, then sends SIGKILL and waits up to 5 seconds. A force stop or
process death skips that escalation, so a wedged child and its duplicate TUN
descriptor can survive.

## Platform Integration

Privileged operations use a pinned Shizuku publication and effective UID. A
permission answer is accepted only for the publication and request code that
issued it. The exact `NetworkRequest` is released through its retained handle
under the same effective UID; `unregisterNetworkCallback` is not used as a
substitute. Hidden APIs and compatibility assumptions are inventoried in
[`mobile/src/hiddenApiStubs/README.md`](../../mobile/src/hiddenApiStubs/README.md)
and the root [`README.md`](../../README.md).

The app calls `TestNetworkManager.createTunInterface`, never
`setupTestNetwork`, and publishes its own restricted `NetworkAgent` with:

- `TRANSPORT_TEST` and the session's exact `TestNetworkSpecifier`;
- no `NOT_RESTRICTED`, `TRUSTED` or `INTERNET` capability;
- an empty allowed-UID set, legacy type `TYPE_TEST`, and score 1.

The session's interface contract is immutable:

| Property | Value |
| --- | --- |
| IPv4 | `192.0.2.1/30` |
| IPv6 | `2001:db8:1::1/64` |
| Virtual DNS | `192.0.2.2`, `fd00::53` |
| MTU | 1500 |

The IPv6 interface prefix must appear globally routable because tethering copies
only global-preferred `/64`s
([AOSP](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#239)).
The documentation prefixes cannot collide with destinations clients need to
reach. Exact routes, DNS servers and cleanup are catalogued in
[`routing.md`](routing.md#rootless-shizuku-mode).

The mode calls `ITetheringConnector.setPreferTestNetworks` directly so the
result code is observed; `TetheringManager` discards it
([AOSP](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#2241)).
Android 13 is supported only when automatic upstream selection is enabled.
Tethering-service death clears the in-service preference but is terminal for the
app process because `TetheringManager` cannot reacquire its cached connector;
restart the app before starting another session.

The observed tethering upstream produces four states:

| State | Meaning | Admits traffic |
| --- | --- | --- |
| `ARMED` | no upstream | no |
| `VERIFYING` | upstream cannot yet be classified | no |
| `ACTIVE` | exact session `Network` selected | yes |
| `RESTART_REQUIRED` | ordinary upstream still selected | no |

## Startup And Withdrawal

Startup performs, in order:

1. retry inherited cleanup;
2. authorize Shizuku and reject unsupported Android 13 configuration, latched
   tethering death, an existing session, or a TestNetwork collision;
3. create the TUN, pin the tethering connector, start upstream observation,
   launch and authenticate the child, and transfer a duplicate TUN descriptor;
4. set the global preference, register the exact request, then register and
   validate the agent;
5. publish the initial state and await the first configuration ACK.

The exact request has a one-minute platform lifetime. Its `onUnavailable` is the
terminal for native-network creation failures that ConnectivityService otherwise
only logs ([AOSP](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#13527)).
All startup failures enter the same rollback.

Withdrawal is idempotent, resumable and non-cancellable:

1. stop observers, close daemon admission and await its ACK; fence the child if
   that cannot be confirmed;
2. clear `preferTestNetworks` and unlink the pinned connector death recipient;
3. terminate the child and confirm exit;
4. unregister the agent and await the callbacks proving the network and exact
   request lost;
5. close the TUN, then release the retained request handle.

An unconfirmed privileged release is retried before another start. An `UNKNOWN`
request or agent blocks in-process recovery because a native network may remain;
process death is required. Tethering connector death requires an app restart.

## Configuration And Handover

`StartShizukuSessionCommand` is the event call that owns the session. It carries
the TUN descriptor, interface name, MTU, virtual DNS addresses and gateway
addresses; the daemon verifies the descriptor before acknowledging readiness.

Each subsequent `ShizukuSessionConfig` is sent directly as a one-shot command
correlated by `call_id`. An ACK contains no duplicate configuration state and is
sent only after daemon-owned retirement completes. Refusal returns an
`ErrorFrame` for that config call and ends the session.

`ShizukuSessionConfig` contains:

- `admit`, true only in `ACTIVE`;
- `upstream_network` and its interface index, or neither when no egress is
  selectable;
- `upstream_generation`, advanced whenever the selected network or its pinned
  properties change.

Changing `admit` retires nothing. While false, ingress is dropped and no state
is created or refreshed; existing deadlines and protocol endings continue.
Downstream membership is not observed.

Advancing `upstream_generation` fences queued output and retires TCP, UDP and
Echo state bound to the old `Network`. Workers are cancelled and joined before
their descriptors and reservations are released. Reassembly and DNS-over-TCP
transports remain; a resolver answer from the old generation becomes SERVFAIL
for that query. A submitted query remains transaction-owned until settlement even
if its transport closes; Android resolver work cannot be cancelled or joined.
There is no fallback network.

## App-UID Dataplane

All TUN input is untrusted and has no physical-client identity. Exact virtual
destinations are classified before reassembly or transport dispatch.

| Traffic | Handling | Owned state |
| --- | --- | --- |
| TCP | terminated by smoltcp and reconnected upstream | socket, bounded bridge, worker and upstream descriptor |
| UDP | endpoint-independent, address-filtered mapping per TUN-visible source | mapping, remotes and bounded send history |
| Virtual DNS | terminated into Android's resolver over UDP or TCP | submitted transaction, resolver descriptor and precharged exchange buffers |
| ICMP Echo | relayed through ping sockets | Echo session and socket |
| Supported ICMP errors | translated only when the quoted flow is proven | no persistent row |
| Other traffic | dropped | none |

MTU 1500 is checked once and drives stack sizing and output fragmentation. Path
MTU errors come from socket error queues and `EMSGSIZE`, not a cached upstream
MTU. Reassembly, fragment identifiers, queues and all traffic-driven tables are
bounded; incomplete fragments and IPv4 fragment-identity reuse use a 60-second
window.

Admission measures descriptor headroom from `RLIMIT_NOFILE` and byte headroom
from `MemAvailable` at session start. General traffic cannot consume the DNS
reserve for one maximum resolver exchange. DNS has no fixed query-count cap:
concurrency is bounded by measured descriptor and memory headroom, with table
capacities derived so they cannot fail first. Resources are charged before
acquisition; reservations release only after covered resources are closed or
dropped and any owning worker is joined. See [`dns.md`](dns.md).

Outer idle floors are:

| State | Floor |
| --- | --- |
| UDP mapping and remotes | 300 s idle |
| UDP error history | 60 s absolute |
| TCP established or able to carry data | 7,440 s idle |
| TCP opening or no longer able to carry data | 240 s idle |
| smoltcp `TIME-WAIT` | its fixed 10 s close delay |
| Echo, incomplete reassembly, IPv4 fragment-ID reuse | 60 s |

TCP half-close preserves byte order and backpressure. After upstream I/O
completes normally, the daemon closes the upstream socket but retains the
client-facing TCP state until its closing handshake completes. Expiry,
generation retirement or session shutdown may still end it abortively. A reset
is acted on only after the TCP stack accepts it; flow identity is socket handle
plus incarnation so handle reuse cannot retire a successor.

## External State And Cleanup

This mode issues no root-daemon command, netlink or netfilter write, `ndc`
request or sysctl change. `CleanRoutingCommand` does not apply. The complete
external-state table, including trigger, exact state, normal stop, process death
and Clean behavior, is in [`routing.md`](routing.md#rootless-shizuku-mode).

Ordered stop normally removes the TUN, child, agent/native network, exact request
and global preference. Process death is weaker:

- a healthy child exits on control EOF, but a wedged child and its TUN descriptor
  can survive;
- `preferTestNetworks` can remain set until a later owned session clears it or
  the tethering service/reboot resets it;
- Android 17 qualification observed stale `testtunN` counter rules in netd's
  shared `tetherctrl_counters` chain. They clear on reboot and must not be deleted
  by the app because the chain also contains live platform state.

Android forwarding, inner IPv4 NAT and delegated IPv6 prefix are
framework-owned consequences of tethering selecting the TestNetwork. Losing the
network makes tethering select another upstream; the app does not cycle tethering
or clean platform-owned forwarding directly.

## Security Boundary

This mode protects tethered traffic from the external network, not from other
apps on the phone. Qualification verified that restricted `Network`-handle
selection is enforced: another UID cannot request, bind to, mark, resolve or ping
through the restricted network. It also verified that any app with network
access can inject packets by naming the TUN interface directly. Such an app can
send traffic through the selected upstream and impersonate a tethered source,
including bypassing a VPN's per-app exclusion, but cannot read the TUN or receive
the downstream-routed replies.

Preventing interface injection requires system/root capabilities this mode does
not have. The dataplane therefore treats all input as hostile and bounds parsing,
reassembly and allocation. Root mode does not have this limitation.

## Failure Semantics

- Startup and publication failures are terminal and roll back acquired state.
- A committed session ends on daemon/control failure, unusable TUN output,
  agent or exact-request loss, or tethering connector death.
- Malformed/unsupported packets, ordinary remote failures, admission refusal and
  resolver outcomes such as `EBUSY` are per-operation results, not fatal reports.
- Rollback never stops Android tethering and cannot remove the process-wide
  `TestNetworkService` singleton.

See [`errors.md`](errors.md) for report delivery and [`dns.md`](dns.md) for
resolver failure classification.

## Qualification

Manual qualification used Android 17 debug builds. This repository has no
instrumented tests.

| Area | Verified | Topology |
| --- | --- | --- |
| Publication | Restricted agent, exact specifier, selection as tethering upstream, delegated `/64`, MTU 1500 | root- and shell-backed Shizuku; USB tethering |
| Admission | `ARMED` TUN injection was dropped without creating dataplane state | root-backed Shizuku; direct TUN input |
| Client traffic | IPv4/IPv6 ping, UDP DNS and HTTPS from an external client | root- and shell-backed Shizuku; USB tethering |
| VPN egress/handover | Full-tunnel VPN egress; old TCP reset and fresh connections succeeded across off/on cycles; no stale resolver state under UDP/DNS load | root-backed Shizuku; USB tethering |
| Root coexistence | Root policy rules won; starting or stopping either mode changed no state owned by the other | root-backed Shizuku; USB tethering |
| Dataplane | IPv4 PMTU, 8 KiB IPv4/IPv6 fragmentation, TCP half-close, malformed-DNS recovery and 16 concurrent 512 KiB downloads | root-backed Shizuku; one USB client |
| Failure/cleanup | Killing the child removed the network; force-stop removed the network but left the preference; normal stop removed network, request, TUN, child and service | root- and shell-backed Shizuku |
| Security | Restricted handle selection held; direct interface injection succeeded | separately signed attacker harness and TUN capture |

## References

- [`shizuku/` Kotlin](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/)
  and [`shizuku/` Rust](../../mobile/src/main/rust/vpnhotspotd/src/shizuku/)
- [`routing.md`](routing.md#rootless-shizuku-mode),
  [`lifecycle.md`](lifecycle.md#app-uid-session), [`dns.md`](dns.md),
  [`errors.md`](errors.md), [`traffic.md`](traffic.md)
- [`daemon.proto`](../../mobile/src/main/proto/daemon.proto)
