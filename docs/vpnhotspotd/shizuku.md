# Shizuku Mode

Shizuku mode shares the app UID's ordinary Android network policy with tethered
clients without root. It publishes a restricted `TRANSPORT_TEST` network over
an app-owned TUN, asks Android tethering to prefer it, and relays traffic from
an app-UID child process. It can run on Android 11 (API 30) or later only when
the installed Mainline modules expose the complete runtime-probed API shape;
the base SDK level alone does not establish availability.

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
- Concurrent `TRANSPORT_TEST` networks are unsupported and not detected. Android
  may select any of them, so tethered traffic can bypass this mode's TUN
  ([AOSP](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/UpstreamNetworkMonitor.java#333)).

## Ownership And Lifecycle

| Owner | State |
| --- | --- |
| [`ShizukuTestNetwork`](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/ShizukuTestNetwork.kt) | TUN, agent, exact request, tethering connector and preference, child, tethering-upstream observation and cleanup ledger |
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
issued it. Its preflight requires `MANAGE_TEST_NETWORKS` and `NETWORK_SETTINGS`,
while accepting either `CONNECTIVITY_USE_RESTRICTED_NETWORKS` or
ConnectivityService's legacy `CONNECTIVITY_INTERNAL` fallback for restricted
networks. The exact `NetworkRequest` is released through its retained handle
under the same effective UID; `unregisterNetworkCallback` is not used as a
substitute. Hidden APIs and compatibility assumptions are inventoried in
[`mobile/src/hiddenApiStubs/README.md`](../../mobile/src/hiddenApiStubs/README.md)
and the root [`README.md`](../../README.md).

The app calls `TestNetworkManager.createTunInterface`, never
`setupTestNetwork`, and publishes its own restricted `NetworkAgent` with:

- `TRANSPORT_TEST` and the request's exact specifier object: a
  `StringNetworkSpecifier` on API 30 or `TestNetworkSpecifier` on API 31+;
- no `NOT_RESTRICTED`, `TRUSTED` or `INTERNET` capability;
- legacy type `TYPE_TEST` and integer score 1; and
- on API 31+, `NOT_VCN_MANAGED` and an empty allowed-UID set. API 30 uses the
  older restricted-network permission model.

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

The mode resolves and invokes `ITetheringConnector.setPreferTestNetworks`
reflectively because a newer tethering APEX can supply it on an older base
release. The listener result is still observed; `TetheringManager` discards it
([AOSP](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#2241)).
Android 11-13 are supported only when automatic upstream selection is enabled.
Tethering-service death clears the in-service preference but is terminal for the
app process because `TetheringManager` cannot reacquire its cached connector;
restart the app before starting another session.

The observed tethering upstream produces three states:

| State | Meaning | Admits traffic |
| --- | --- | --- |
| `ARMED` | no upstream | no |
| `ACTIVE` | exact session `Network` selected | yes |
| `RESTART_REQUIRED` | some other upstream selected | no |

## Startup And Withdrawal

Startup keeps only required dependencies ordered:

1. retry inherited cleanup;
2. resolve the compatibility-sensitive runtime methods, members, constructors
   and agent lifecycle shape before authorization;
3. authorize Shizuku in parallel with the remaining read-only session gates;
4. preflight the remaining cleanup-critical reflection, then create the TUN;
5. in parallel, acquire the tethering connector, wait for the first upstream,
   launch and authenticate the child, transfer its TUN descriptor, start the
   observers, and build the network configuration;
6. set the global preference, register the exact request, then register the
   agent;
7. await request-available, capabilities and link-property callbacks in
   parallel, plus `onNetworkCreated` when the runtime agent supports it, then
   validate the publication; and
8. publish the initial state and await the first configuration ACK.

The exact request has a one-minute platform lifetime. Its `onUnavailable` is the
terminal for native-network creation failures that ConnectivityService otherwise
only logs ([AOSP](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#13527)).
All startup failures enter the same rollback.

Withdrawal is idempotent, resumable and non-cancellable:

1. stop observers and close daemon admission;
2. in parallel, stop the child and clear `preferTestNetworks`; unregister the
   agent only after the clear attempt settles;
3. await agent `unwanted` and request `lost`; when the runtime exposes the
   created/destroyed callback pair (baseline API 31+), also re-check creation
   and await `destroyed`;
4. if both cleanup lanes succeed, close the TUN; otherwise retain it and the
   cleanup ledger for a later retirement attempt;
5. after TUN closure, retry any unconfirmed preference clear and release the
   retained request in parallel, then clean up the local callback.

When the runtime exposes the created/destroyed pair, `destroyed` is the hard
native-network fence before TUN closure
([AOSP](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java#1158)).
Without that pair (including baseline API 30), `unwanted` plus request `lost`
is the strongest observable withdrawal, but both can precede native-network
destruction. No delay or polling is used to pretend otherwise. The `lost`
re-check is best-effort because agent and request callbacks use different Binder
channels. Both cleanup lanes settle before the first failure in issue order is
rethrown with later failures suppressed, deferring TUN closure and privileged
cleanup to a later retirement attempt.

An unconfirmed privileged release is retried before another start. An `UNKNOWN`
request or agent blocks in-process recovery because a native network may remain;
process death is required. Tethering connector death requires an app restart.

## Configuration And Admission

`StartShizukuSessionCommand` is the event call that owns the session. It carries
the TUN descriptor, interface name, MTU, virtual DNS addresses and gateway
addresses; the daemon verifies the descriptor before acknowledging readiness.

Each subsequent `ShizukuSessionConfig` is sent directly as a one-shot command
correlated by `call_id`. It contains only `admit`, true in `ACTIVE` and false in
every other state. An ACK contains no duplicate configuration state and is sent
only after TUN ingress has applied that admission value. Refusal returns an
`ErrorFrame` for that config call and ends the session.

The Rust config handoff has depth one because Kotlin coalesces changes to one
pending value and does not send another call before the current ACK. An occupied
slot makes the control reader wait; a closed slot refuses the call and ends the
session rather than buffering another configuration.

Changing `admit` tears down nothing. While false, ingress is dropped and no state
is created or refreshed; existing deadlines and protocol endings continue.
Downstream membership is not observed.

The daemon neither selects an Android `Network` nor binds its process or egress
sockets to one. `ProcessBuilder` starts it through fork and exec, so the new
image begins with libnetd_client's process and resolver selections unset; the
daemon leaves them unset
([process launch](https://android.googlesource.com/platform/libcore/+/refs/tags/android-17.0.0_r1/ojluni/src/main/java/java/lang/UNIXProcess.java#67),
[network state](https://android.googlesource.com/platform/system/netd/+/refs/tags/android-17.0.0_r1/client/NetdClient.cpp#61)).
Android therefore routes its sockets as ordinary sockets created by the app UID:

- a new TCP connection uses the UID's policy at `connect`; an established
  connection keeps Android's normal connected-socket behavior and is not ended
  merely because the app's default network changes
  ([AOSP](https://android.googlesource.com/platform/system/netd/+/refs/tags/android-17.0.0_r1/server/NetworkController.cpp#282));
- UDP and Echo use unconnected, unbound sockets, so Android policy routing
  chooses the path for their sends; existing mappings and Echo sessions are not
  reset on a default-network change
  ([AOSP](https://android.googlesource.com/platform/system/netd/+/refs/tags/android-17.0.0_r1/server/FwmarkServer.cpp#237));
- each DNS submission passes `NETWORK_UNSPECIFIED` to `android_res_nsend`.
  This is an unset selection, not a handle naming the current default. The DNS
  proxy obtains the peer UID and chooses the applicable VPN DNS network or the
  UID/default DNS network for that submission
  ([unset selection](https://android.googlesource.com/platform/frameworks/native/+/refs/tags/android-17.0.0_r1/include/android/multinetwork.h#47),
  [submission](https://android.googlesource.com/platform/system/netd/+/refs/tags/android-17.0.0_r1/client/NetdClient.cpp#519),
  [UID context](https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-17.0.0_r1/DnsProxyListener.cpp#1024),
  [selection](https://android.googlesource.com/platform/system/netd/+/refs/tags/android-17.0.0_r1/server/NetworkController.cpp#217)).

An in-flight resolver request remains transaction-owned until settlement even
if its transport closes or the default network changes; Android resolver work
cannot be cancelled or joined. Android may deliberately fall through to the
physical default for a VPN-excluded or otherwise unrouted destination, and may
use default-network DNS when the VPN supplies none. `VpnService.allowBypass()`
separately permits explicit selection of other networks
([AOSP](https://android.googlesource.com/platform/system/netd/+/refs/tags/android-17.0.0_r1/server/RouteController.cpp#712)).

## App-UID Dataplane

TUN input has no physical-client identity and is validated before transport
dispatch. Virtual destinations are classified before and after any required IPv6
normalization or reassembly. Each successful round removes headers from the
finite packet; no transformation quota is needed. Virtual DNS accepts only TCP
or UDP port 53. Malformed, unsupported, source-routed, or differently addressed
traffic is dropped.

| Traffic | Handling | Owned state |
| --- | --- | --- |
| TCP | terminated by smoltcp and reconnected upstream | smoltcp socket, worker, upstream descriptor and bounded per-flow buffers |
| UDP | endpoint-independent, address-filtered mapping per TUN-visible source | mapping/socket, dynamic contacted-remotes and endpoint hop-limit history |
| Virtual DNS | terminated into Android's resolver over UDP or TCP | dynamic transaction, resolver descriptor and ordinary query/result buffers |
| ICMP Echo | relayed through ping sockets | dynamic Echo sessions and one opened ping/reply socket per family |
| Supported ICMP errors | UDP repeats route-level errors for a contacted destination and datagram-specific errors when the kernel-correlated endpoint has one exact retained hop limit; Echo requires a live rewritten-sequence match | UDP mapping history or Echo session |
| Other traffic | dropped | none |

One task owns TUN ingress and selects on every dataplane source at once, biased:

| Arm | Metering |
| --- | --- |
| Cancellation | first and unmetered |
| Configuration | prioritized and unmetered |
| UDP/Echo completions and virtual DNS settlement | unmetered; bounded by metered producers |
| TCP attention: traffic, terminals, DNS transactions and client closes | one turn per pass |
| TCP DNS handoffs | one turn per pass |
| UDP/Echo replies and TUN readability | one turn per pass, and only while the interface handoff accepts |
| UDP mapping, Echo session, fragment and TCP idle deadlines | one turn per pass while the interface handoff accepts; unmetered and non-emitting while it is full |

A metered source takes at most one turn per pass. When all remaining sources are
pending, the pass resets in place. A carried retry avoids immediately repeating
the O(N) TCP-attention scan. Configuration is prioritized and limited to one
in-flight call; completion arms are bounded by metered producers. Fairness is
therefore among metered turns, not a wall-clock guarantee against control work.

While the interface handoff is full, output-producing arms are gated and the
pass cannot reset. Recurring deadlines still retire due rows without taking a
turn or emitting, preserving both expiry and the interrupted fairness state.
This mode comes from the turn's capacity snapshot, so a deadline cannot steal a
slot the writer frees concurrently. Fragment expiry counts suppressed
best-effort errors as `fragments-stalled`; TCP expiry leaves resets for the first
accepting TCP turn. UDP and Echo also check deadlines when consuming a reply or
error, independent of sweep order.

Android applies the app UID's routing and access policy to every unbound egress
socket; the daemon adds no network or ingress-interface authorization of its
own. UDP accepts a reply only for the current live mapping and a remote address
that mapping contacted. Echo requires the current ping-socket worker and a live
remote/translated-sequence session. Worker identities reject events already
queued by sockets that are no longer current. After the kernel reuses a UDP port
or ping identifier, however, a late or forged packet delivered to that live
socket and matching the remaining correlation is indistinguishable; this mode
provides no identity-quarantine or ingress-interface-provenance guarantee.

MTU 1500 is checked once and drives stack sizing and output fragmentation. Path
MTU errors come from socket error queues and `EMSGSIZE`, not a cached upstream
MTU. Egress socket state is what keeps that true per family:

| Family | Fragmentation state | Installed |
| --- | --- | --- |
| IPv4 UDP mapping and ping | `IP_MTU_DISCOVER` = `IP_PMTUDISC_DO` or `IP_PMTUDISC_OMIT`, from the relayed packet's own DF bit | before each send |
| IPv6 UDP mapping and ping | `IPV6_DONTFRAG` = 1 | at socket creation |

`IPV6_DONTFRAG` makes oversized IPv6 sends fail with `EMSGSIZE`, allowing the
existing error-queue path to return ICMPv6 Packet Too Big instead of emitting
source fragments. Failure to set descriptor-lifetime options aborts socket
creation; failure to apply IPv4's per-send policy reports and drops that packet.
The state is descriptor-local: normal stop or daemon exit removes it by closing
the socket; a surviving child retains it until exit. `CleanRoutingCommand` has no
persistent state to remove and cannot close that child's descriptors.

### Descriptor Admission And Dynamic State

Resource admission is descriptor-only. At session start the daemon reads the
soft `RLIMIT_NOFILE`, counts the descriptors already visible in `/proc/self/fd`
(including the counting directory handle), and admits against the difference.
Invalid arithmetic or a total below the one-unit DNS floor fails startup; a
total of one permits DNS only. The conservative count leaves one descriptor
below the soft limit at full admission. A TCP candidate is opened and either
charged or closed within the same owner turn.

Exactly one unit inside that measured total is protected for a DNS resolver
descriptor. It is neither pre-opened nor a DNS concurrency cap: DNS may use
other free units, while general work cannot take the last one. UDP mappings, TCP
upstreams, Echo family sockets, and submitted resolver descriptors each hold one
unit. TCP DNS reserves before allocating an admitted body and releases the unit
if submission returns no descriptor. Denial affects only the new owner; an
answerable DNS query receives SERVFAIL. A worker's descriptor is closed and the
worker joined before release, although memory-only client TCP state may remain.
See [`dns.md`](dns.md).

There is no daemon aggregate memory share, `MemAvailable` fraction, modeled byte
ledger or byte precharge. Memory-only flow, DNS, fragment, Echo, UDP-history and
IPv4 Identification state grows on demand; descriptor-owning rows remain
indirectly bounded by admission. Exhaustion may terminate the recoverable child,
and disconnecting the downstream or restarting the session/process clears the
state. This trusted-downstream boundary is preferred to arbitrary table quotas.

That trusted-downstream recovery boundary covers state a *downstream client*
creates, not queues an Internet peer can fill before owner authorization. UDP
and Echo reply mailboxes and the TUN datagram handoff are therefore one-slot;
producers wait before reading or refuse a complete datagram. See [Bounded
Buffers And Handoffs](#bounded-buffers-and-handoffs).

Timed rows are indexed by deadline rather than scanned. UDP mappings and their
contacted-remote histories, Echo sessions, fragment-reassembly contexts and TCP
flow idle floors each keep one ordered entry per armed row. Earliest-deadline
queries and updates are logarithmic, while expiry removes only due rows. UDP and
Echo still validate the row's deadline when consuming a reply or error, so
liveness does not depend on the sweep running first.

Echo additionally indexes live remotes by family and rewritten sequence, which
lets a quoted ICMP error resolve to exactly one request or report ambiguity
without a table scan. It expires due rows before reading that cardinality. The
client-facing stack caches its absolute next-poll deadline and recomputes it only
after stack activity, socket-set changes, bridge progress, or abort. Pinned
smoltcp 0.13.1's
[`Interface::poll_delay`](https://github.com/smoltcp-rs/smoltcp/blob/v0.13.1/src/iface/interface/mod.rs)
walks the socket set's high-water backing store, including removed-socket holes;
the round-robin bridge pump remains linear in live flows.

UDP remote ICMP translation first requires the contacted-destination
authorization. That is sufficient for route-level Packet Too Big, Fragmentation
Needed and transit-timeout errors. Destination Unreachable also requires exact
per-endpoint hop-limit evidence. Linux identifies the mapping socket before
queueing the error ([IPv4 lookup](https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/net/ipv4/udp.c#735),
[IPv6 lookup](https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/net/ipv6/udp.c#588)), and
[`IP_RECVERR`](https://man7.org/linux/man-pages/man7/ip.7.html) supplies the
original destination, so no per-packet payload is retained. Repeated equal hop
limits remain exact; differing values make the endpoint ambiguous and suppress
datagram-specific translation. Endpoint rows share their IP's 300-second
authorization deadline and have no separate count or byte limit.

IPv4 Identification state has no count, byte or tuple cap: a
`(source, destination, protocol)` row is created the first time that tuple needs
one and all rows are dropped with the session. [RFC 6864 section
4.3](https://www.rfc-editor.org/rfc/rfc6864.html#section-4.3) forbids reuse within
one maximum datagram lifetime because it can cause mis-reassembly. Each tuple
therefore issues all 65,536 values once and starts another cycle only after all
issued datagrams settle and 120 seconds pass after its last successful fragment
write. RFC 6864 [section
5.2](https://www.rfc-editor.org/rfc/rfc6864.html#section-5.2) calls 120 seconds a
typical interpretation of the otherwise undefined lifetime; [RFC 1122 section
3.3.2](https://www.rfc-editor.org/rfc/rfc1122.html#page-58) gives a 60–120 second
reassembly range, so the daemon uses its conservative upper endpoint.

A value is only issued for output the daemon is about to fragment, which is
oversized IPv4 output alone: [RFC 6864 section
4.1](https://www.rfc-editor.org/rfc/rfc6864.html#section-4.1) places no reuse
constraint on atomic datagrams, and IPv6 uses a separate 32-bit field. A value is
returned when no fragment can reach the wire; a partial write consumes it and
dates the window from the last successful write. Because predecessor wire state
is unavailable, a new session also quarantines issuance for 120 seconds. A
quarantined or exhausted tuple silently drops and counts only oversized IPv4
output as `identification-denied`; atomic IPv4 and all IPv6 output continue.

Output counters say what really happened to a datagram and stop where this
owner can know. `tun output` distinguishes queued, refused, unbuildable and
Identification-denied packets; only `tun egress` counts successful descriptor
writes. UDP and Echo `queued`/`unqueued` counters likewise describe handoff
admission, not delivery.

### Bounded Buffers And Handoffs

The remaining fixed resource bounds come from local backpressure, exact configured
need, protocol fields, or directly analogous maintained upstream defaults; they
are not shares of an aggregate memory budget:

| Resource | Bound and derivation | Exhaustion behavior |
| --- | --- | --- |
| smoltcp interface address slots | Two, explicitly selecting pinned smoltcp 0.13.1's maintained [`IFACE_MAX_ADDR_COUNT` default](https://github.com/smoltcp-rs/smoltcp/blob/v0.13.1/build.rs#L9), exactly matching this engine's one IPv4 and one IPv6 interception address. | Both slots are populated once at engine creation. An additional or otherwise refused push is reported and that address is not installed; existing addresses remain. |
| smoltcp TCP out-of-order ranges | Four discontinuous receive ranges per socket, explicitly selecting pinned smoltcp 0.13.1's maintained [`ASSEMBLER_MAX_SEGMENT_COUNT` default](https://github.com/smoltcp-rs/smoltcp/blob/v0.13.1/build.rs#L16). | When a segment would require an unmergeable fifth range, [smoltcp discards that segment](https://github.com/smoltcp-rs/smoltcp/blob/v0.13.1/src/socket/tcp.rs#L2141-L2151); existing ranges remain and normal TCP acknowledgement/retransmission recovers the missing data. |
| smoltcp TCP receive and transmit buffers | 65,535 bytes (`u16::MAX`) in each direction: [RFC 9293 section 3.1](https://www.rfc-editor.org/rfc/rfc9293.html#section-3.1) defines the unscaled TCP Window as 16 bits, and the pinned smoltcp 0.13.1 maintained [streaming server example](https://github.com/smoltcp-rs/smoltcp/blob/v0.13.1/examples/server.rs#L77-L83) uses that capacity for both directions. | A full receive buffer reduces the advertised window, possibly to zero; a full transmit buffer stops accepting writes. Bytes are not evicted. |
| Upstream/client bridge | 65,535 bytes in each direction, equal to the adjacent smoltcp direction so the handoff is not narrower than the stack buffer | A full bridge suspends its writer and propagates lossless TCP backpressure. |
| Client terminal tail | 65,535 bytes, exactly the smoltcp receive-buffer capacity whose accepted tail may remain when upstream I/O ends | Tail extraction must fit by construction; an invariant failure aborts the flow rather than truncating accepted bytes. |
| Socket/DNS TCP read scratch | 8,192 bytes per read direction, the `DEFAULT_BUF_SIZE` used by pinned Tokio 1.53.1's [`copy` utilities](https://github.com/tokio-rs/tokio/blob/tokio-1.53.1/tokio/src/io/util/mod.rs#L88) | Filling it completes that read and the readiness loop continues; it is not a queued-byte limit. |
| Socket error-queue quote scratch | 8 bytes per owner, exactly one ICMP Echo header. Android/Linux [`ping_err`](https://android.googlesource.com/kernel/common/+/e8c92d268b8b8feb550ca8d24a92c1c98ed65ace/net/ipv4/ping.c#483) starts the queued payload at that header, which contains the rewritten sequence the Echo owner matches. | Later payload bytes are truncated because they cannot affect Echo correlation. A shorter Echo quote cannot identify a live session and is dropped; UDP uses the socket and original destination metadata, not the quoted payload. |
| Per-flow DNS control handoffs | One message in each direction because a transport processes exactly one framed query at a time | The protocol never legitimately needs a second slot. A refused query-to-owner handoff ends the transport. A second owner-to-transport control is discarded and counted unreachable; closure means that transport has already ended. No channel grows. |
| UDP/Echo reply mailboxes | One event per subsystem, the minimum Tokio `mpsc` capacity, because the owner consumes at most one UDP and one Echo reply per fair pass. With one event being processed and both slots refilled, at most three remote payloads are daemon-owned. | Workers reserve after readiness but before reading or allocating. When full, data remains in the socket receive queue under kernel drop policy. Cancellation or owner closure releases the reservation wait without a read. |
| TUN writer handoff | One complete logical datagram, the minimum Tokio `mpsc` capacity. The serial writer may own one additional in-progress datagram, so at most two are daemon-owned. | A full handoff drops and counts the entire non-TCP datagram, never a fragment prefix. Reply events remain queued while full. TCP instead stops polling smoltcp, retaining data and resets in its sockets. Writer closure ends the session; accepted batches are written serially without interleaving. |
| Client-facing stack device slots | One inbound and one outbound packet, each at most the interface MTU. Ingress pushes and settles synchronously; pinned smoltcp 0.13.1 can fill at most the one transmit slot before the loop drains it. | Known output exhaustion gates the TUN read. A post-read ingress refusal drops and counts `unconsumed`, unwinding tentative state; this covers a stale occupied input slot or a reset pre-settlement consuming the last output slot. A busy output slot makes smoltcp retain and retry the packet. |
| IPv4 Identification settlement handoff | One queued ending, because the writer settles each datagram before receiving the next; at most one additional ending can be blocked in the writer. | A full slot backpressures the writer. The owner prioritizes settlement even while waiting for writer capacity, avoiding deadlock; cancellation or closure ends the wait. Losing an ending would prevent tuple reuse for the session. |

Aggregate TCP-DNS requests use an unbounded session-owned owner handoff rather
than an arbitrary queue depth. It carries no remote payload, each descriptor-
admitted transport is sequential, and the owner consumes one request per fair
pass. Cancellation is checked before publication; owner closure ends transport.

Reply workers reserve their mailbox before reading. The TUN writer validates and
serializes whole batches; a partial final batch settles IPv4 Identification from
its last successful write. TUN ingress and smoltcp polling require handoff
capacity, and a reset candidate rechecks after its pre-settlement poll. The owner
prioritizes Identification settlements even while waiting for that capacity.

### Fragment And Protocol Bounds

Incomplete IPv4 and IPv6 fragment contexts grow on demand without a separate
byte or context-count ceiling. The Linux fragment thresholds are not reused:
Linux accounts `sk_buff` truesize and queue structures that this Rust
representation does not own. Completion, rejection, expiry or session stop
removes a context; allocator exhaustion follows the process-recovery policy.

Fragment lifetime follows Linux by family: IPv4 expires after 30 seconds from
[`IP_FRAG_TIME`](https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/include/net/ip.h#146),
while IPv6 expires after 60 seconds from
[`IPV6_FRAG_TIMEOUT`](https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/include/net/ipv6.h#548).
Expiry discards the context; when fragment zero supplies a quotable header, the
daemon may return the corresponding reassembly-timeout ICMP error.

Protocol field widths bound each context. IPv6 reassembly accepts no fragment
whose body ends past 65,535 bytes, the widest value of the 16-bit Payload Length
field in [RFC 8200 section 3](https://www.rfc-editor.org/rfc/rfc8200.html#section-3);
[section 4.5](https://www.rfc-editor.org/rfc/rfc8200.html#section-4.5) requires
discarding larger results. IPv4 header plus body must fit its 65,535-byte Total
Length; before fragment zero, the context reserves the 20-byte minimum header.
The four-bit IHL permits at most 60 bytes ([RFC 791 section
3.1](https://www.rfc-editor.org/rfc/rfc791.html#section-3.1)). A later header that
no longer fits, inconsistent final length, overlap, or data beyond the declared
end discards the context rather than truncating it.

Each opened ping family has one 65,535-byte receive buffer; IPv6 jumbograms are
unsupported. UDP peeks and allocates the exact datagram length. TCP DNS is
limited by its 16-bit length prefix. Generated ICMP errors truncate their quote
so the full IPv4 packet is at most 576 bytes under
[RFC 1812 section 4.3.2.3](https://www.rfc-editor.org/rfc/rfc1812.html#section-4.3.2.3),
and the full IPv6 packet is at most the 1,280-byte minimum MTU under
[RFC 4443 section 2.4](https://www.rfc-editor.org/rfc/rfc4443.html#section-2.4).
Inputs outside these structural bounds are dropped.

Each Echo remote has 65,536 rewritten sequence values, exactly the complete
16-bit ICMP Echo Sequence Number space. If every value for that remote is still
live, a new request is dropped until a reply or the 60-second expiry releases one.

IPv6 extension processing has no header- or transformation-count cap. [RFC 8200
section 4.1](https://www.rfc-editor.org/rfc/rfc8200.html#section-4.1) requires a
destination to accept and attempt extension headers in any order and number,
subject to protocol ordering. One scan validates and measures the prefix, then
one allocation copies the retained packet, making work linear in encoded bytes.
Atomic Fragment headers are consumed in that scan so [RFC 6946 section
4](https://www.rfc-editor.org/rfc/rfc6946.html#section-4) isolation is preserved.

Only a Fragment header that really fragments stops the scan, and only that one
creates a context. Nested genuine fragments therefore proceed one reassembly and
normalization round at a time; every successful round removes at least one
eight-byte header. These downstream-created contexts have no count cap and use
their family expiry. `extension-headers` counts consumed prefix headers.

### Lifetimes

| State | Lifetime and source | Expiry behavior |
| --- | --- | --- |
| UDP mapping, contacted remotes and endpoint history | 300 seconds idle, the [RFC 4787 section 4.3](https://www.rfc-editor.org/rfc/rfc4787.html#section-4.3) default; endpoint rows share their IP deadline | Expiry removes authorization and endpoint evidence; mapping expiry also closes the socket. Point-of-use checks reject overdue replies before sweep. |
| Echo session | 60 seconds from allocation, the [RFC 5508 section 3.2](https://www.rfc-editor.org/rfc/rfc5508.html#section-3.2) minimum | Expiry or a point-of-use check removes the session, so later replies and errors cannot match it. The family socket remains while otherwise needed. |
| IPv4 Identification tuple | The row lasts for the session. Each 65,536-value cycle waits for settlement plus 120 seconds since the last fragment write, the conservative upper endpoint from [RFC 6864 section 5.2](https://www.rfc-editor.org/rfc/rfc6864.html#section-5.2) and [RFC 1122 section 3.3.2](https://www.rfc-editor.org/rfc/rfc1122.html#page-58); a new session also waits 120 seconds. | Quarantine or exhaustion drops and counts oversized IPv4 output only. Rows disappear at session end. |
| IPv4 incomplete reassembly | 30 seconds, Linux `IP_FRAG_TIME` | The context is discarded and may produce ICMP Time Exceeded only when fragment zero was retained. |
| IPv6 incomplete reassembly | 60 seconds, Linux `IPV6_FRAG_TIMEOUT` | The context is discarded and may produce ICMPv6 Time Exceeded only when fragment zero was retained. |
| TCP established or able to carry data | 7,440 seconds idle, the two-hour-four-minute minimum in [RFC 5382 REQ-5](https://www.rfc-editor.org/rfc/rfc5382.html#section-5) | The expired flow is torn down; any upstream descriptor and lease, plus its buffers, are released. |
| TCP opening or no longer able to carry data | 240 seconds idle, the four-minute transitory minimum in RFC 5382 REQ-5 | The expired flow is torn down; any upstream descriptor and lease, plus its buffers, are released. |
| smoltcp `TIME-WAIT` | The pinned smoltcp 0.13.1 [fixed 10-second close delay](https://github.com/smoltcp-rs/smoltcp/blob/v0.13.1/src/socket/tcp.rs#L297) | Only memory-only client-facing state remains until smoltcp closes it, then the flow record is released; the upstream descriptor and lease ended with its worker. |

TCP half-close preserves byte order and backpressure. After upstream I/O
completes normally, the daemon joins the worker, closes the upstream socket and
immediately releases its descriptor lease, but retains the memory-only
client-facing state through its closing handshake. An aborted socket is likewise
retained until its reset is emitted if the interface was full. Expiry, explicit
teardown or session shutdown may end it sooner. Resets take effect only after
stack acceptance; handle plus incarnation prevents stale teardown after reuse.

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
Android forwarding, inner IPv4 NAT and delegated IPv6 prefix are
framework-owned consequences of tethering selecting the TestNetwork. Losing the
network makes tethering select another upstream; the app does not cycle tethering
or clean platform-owned forwarding directly.

## Security Boundary

This mode gives tethered traffic the app UID's external-network policy; it does
not create a stronger receive boundary. Android restricts app-UID ingress to the
VPN interface only for a non-bypassable, fully routed app VPN without excluded
routes. For a bypassable or split/excluding VPN it installs wildcard ingress
acceptance instead
([AOSP](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#12103),
[BPF enforcement](https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/bpf/progs/netd.c#808)).

Qualification verified that another UID cannot select the restricted
TestNetwork through the normal Android `Network` APIs. It also verified that any
app with network access can inject packets by naming the TUN interface directly.
Such an app can send traffic under this app UID's network policy and impersonate
a tethered source, including bypassing a VPN's per-app exclusion, but cannot
read the TUN or receive the downstream-routed replies.

Preventing interface injection requires system/root capabilities this mode does
not have. The dataplane treats packet bytes as untrusted for parser and protocol
correctness and bounds each fragment context by protocol size and its family
expiry. It does not impose per-client or aggregate quotas on ordinary
downstream-created table memory; that state uses the disconnect/session-restart
recovery boundary described above. Queues an Internet peer fills are not in that
class and are bounded structurally instead, because the owner authorizes a reply
only after the handoff and disconnecting a client does not stop a remote sending.
Root mode does not have the interface-injection limitation.

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

## References

- [`shizuku/` Kotlin](../../mobile/src/main/java/be/mygod/vpnhotspot/shizuku/)
  and [`shizuku/` Rust](../../mobile/src/main/rust/vpnhotspotd/src/shizuku/)
- [`routing.md`](routing.md#rootless-shizuku-mode),
  [`lifecycle.md`](lifecycle.md#app-uid-session), [`dns.md`](dns.md),
  [`errors.md`](errors.md), [`traffic.md`](traffic.md)
- [`daemon.proto`](../../mobile/src/main/proto/daemon.proto)
