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
dispatch. Exact virtual destinations are classified before reassembly; IPv6
extension chains and non-initial fragments addressed to virtual DNS are
unwrapped or reassembled, then classified again. Processing continues only
while a successful step removes an extension chain or completes reassembly and
removes a Fragment header, so the finite encoded packet bounds the work without
a transformation quota. Only TCP or UDP port 53 is accepted. Supported IPv6
extension headers are walked until transport or Fragment. Malformed or truncated
chains, unsupported headers including AH and ESP, source routing, other
transports and other ports are dropped rather than relayed from the reserved
address.

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
| UDP/Echo replies, TCP DNS handoffs, deadlines and TUN readability | one turn per pass |

A metered source takes at most one turn per pass. When all remaining sources are
pending, the pass resets without rescanning owner deadlines. If the pending set
included TCP attention (the only O(N) poll), a carried retry first offers only
the sources already served and keeps attention disabled; otherwise all sources
reopen immediately. Configuration is deliberately unmetered because it is
authenticated, limited to one in-flight call and updates state used by later
arms. Settlement and completion arms are unmetered because their work is bounded
by metered producers. An indefinitely ready configuration stream can still delay
ordinary work; fairness is a bound among metered turns, not wall-clock time.

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
(including the directory handle used to count them, conservatively), and admits
against the difference. An unrepresentable difference or a result that does not
contain the DNS floor fails session startup. A total of exactly one runs with
DNS capacity and no general descriptor capacity rather than imposing another
minimum. Counting the directory handle leaves one descriptor below the soft
limit at full admission. A TCP candidate is opened synchronously, then either
charged immediately or closed on denial before another owner turn; admission
therefore never retains a lease for a socket that failed to open.

Exactly one unit inside that measured total is protected for a DNS resolver
descriptor. The floor is capacity, not a descriptor opened in advance and not a
DNS concurrency ceiling: DNS may use any remaining capacity, while general work
cannot consume the last unit. One general unit is held for each live UDP mapping
socket, opened TCP upstream-flow socket and opened Echo ping/reply family socket.
A virtual-DNS TCP transport and its client-facing smoltcp state are memory-only;
only a query accepted for resolver submission holds one DNS unit, which can
authorize at most its one returned resolver descriptor. TCP DNS takes this unit
when the framed length is accepted, before allocating the body, and releases it
if submission never returns a descriptor. A denied general request drops or
refuses only the new mapping, flow or family socket; an answerable denied DNS
query receives SERVFAIL. Existing owners are untouched.
A descriptor-owning worker is joined and its descriptor is closed before its
lease is released. In particular, TCP releases the upstream lease at that worker
terminal even when the memory-only client-facing state remains for a closing
handshake. See [`dns.md`](dns.md).

There is no daemon aggregate memory share, `MemAvailable` fraction, modeled byte
ledger or byte precharge. UDP contacted-remotes and endpoint hop-limit histories,
Echo sessions, queued TUN datagrams, client-facing TCP flows, virtual-DNS TCP
transports, DNS transaction tables, aggregate TCP-DNS owner requests, queued
UDP/Echo reply events, fragment-reassembly contexts and IPv4 Identification
tuples grow on demand without prepared counts or independent per-client limits.
Some owning rows still have a descriptor and are therefore indirectly limited
by descriptor admission; the memory-only rows are not. Real allocator or
Android process-memory exhaustion may terminate the app-UID child. Disconnecting
the downstream, stopping or restarting the session, killing the child, or
rebooting drops all of this process-local state, so this trusted-downstream mode
deliberately prefers that recovery boundary to arbitrary table quotas.

UDP remote ICMP translation first requires the contacted-destination
authorization. That is sufficient for route-level Packet Too Big, Fragmentation
Needed and transit-timeout errors. For a Destination Unreachable, Linux has
already looked up this mapping's socket from the offending UDP tuple before
putting the error in its queue. In the pinned Android 15 6.6 kernel, IPv4 does
the [`udp_err` lookup](https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/net/ipv4/udp.c#735)
before [`ip_icmp_error`](https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/net/ipv4/udp.c#808),
and IPv6 does the [`udpv6_err` lookup](https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/net/ipv6/udp.c#588)
before [`ipv6_icmp_error`](https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/net/ipv6/udp.c#643).
[`IP_RECVERR`](https://man7.org/linux/man-pages/man7/ip.7.html) also
returns the original destination that caused the error. The
mapping therefore retains no per-packet row or payload. It keeps one hop-limit
state per successfully contacted destination socket: repeated sends with the
same value stay exact, while observing a different value makes that endpoint
ambiguous. Only an exact endpoint supplies `Datagram` correlation; ambiguous or
untracked endpoints drop the datagram-specific error rather than inventing the
original hop limit. The endpoint map has no count or aggregate-byte limit and no
separate timer. When an IP's existing 300-second remote authorization expires,
its endpoint evidence is dropped with it; mapping end releases the whole table.
Allocator exhaustion has the recoverable process behavior above. IPv4
Identification state likewise has no timeout, cap, quarantine or refusal: each
`(source, destination, protocol)` tuple gets a freshly keyed initial `u16` and a wrapping sequence, following Linux's
[`__ip_select_ident`](https://github.com/torvalds/linux/blob/master/net/ipv4/route.c)
model without Linux's finite shared bucket table. The tuple map is dropped with
the session.

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
| TUN writer handoff | Unbounded session-owned FIFO of complete logical datagrams awaiting the sole serial writer; each item owns all packets/fragments of one datagram. | Enqueue has no capacity refusal and fails only after the writer receiver closes, when the session is already ending. The writer validates each complete batch before its first write and drains it serially, so datagrams cannot interleave; session stop drops the receiver and every queued batch. |

Aggregate TCP-DNS requests and UDP/Echo replies use unbounded session-owned
owner handoffs rather than daemon-selected queue depths. Each DNS transport
remains sequential; cancellation is checked before each request publication, a
closed owner ends the transport, and the shared owner consumes at most one
request per fair scheduler pass. Each socket worker waits for readiness, checks
cancellation and owner closure, then reads and publishes at most one datagram or
one error per turn. The owner likewise consumes at most one UDP event and one
Echo event per fair pass, and each socket worker yields between completed turns
so continuously ready sockets cannot monopolize the runtime. Socket-worker
closure or cancellation stops further reads; session stop cancels and joins the
workers and drops any queued events.

The TUN writer can own one in-progress datagram in addition to queued batches.
Invalid packetization rejects the whole next batch and reports the packet index,
size and batch size. Cancellation or a fatal TUN write may put a prefix of one
batch on the wire, but the sole writer cannot interleave another datagram with
that prefix.

### Fragment And Protocol Bounds

Incomplete IPv4 and IPv6 fragment contexts grow on demand without a separate
byte or context-count ceiling. The Linux fragment thresholds are not reused:
Linux accounts `sk_buff` truesize and queue structures that this Rust
representation does not own, so copying the numeric threshold would not bound
the same resource. Completion, malformed or overlapping rejection, expiry and
session stop remove the affected retained context. Real allocator exhaustion
has the process-recovery behavior described above.

Fragment lifetime follows Linux by family: IPv4 expires after 30 seconds from
[`IP_FRAG_TIME`](https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/include/net/ip.h#146),
while IPv6 expires after 60 seconds from
[`IPV6_FRAG_TIMEOUT`](https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/include/net/ipv6.h#548).
Expiry discards the context; when fragment zero supplies a quotable header, the
daemon may return the corresponding reassembly-timeout ICMP error.

Protocol field widths bound each context. IPv6 reassembly accepts no fragment
whose body ends past 65,535 bytes, the widest value of the 16-bit Payload Length
field in [RFC 8200 section 3](https://www.rfc-editor.org/rfc/rfc8200.html#section-3);
[section 4.5](https://www.rfc-editor.org/rfc/rfc8200.html#section-4.5)
requires discarding a fragment whose reassembled payload would be larger. IPv4
accepts no context whose actual header plus reassembled body exceeds its 65,535-
byte Total Length field. A headless context reserves space for IPv4's minimum
20-byte header; if fragment zero later supplies options, its larger actual header
applies and may discard the context. Conflicting final lengths, or a non-final
fragment claiming more data at an already declared end, likewise discard the
context rather than truncating the reconstruction. [RFC 791 section
3.1](https://www.rfc-editor.org/rfc/rfc791.html#section-3.1) defines both the
16-bit Total Length and the four-bit IHL in 32-bit words, making 60 bytes the
largest IPv4 header retained for reassembly; a larger claimed header is
malformed. Ping sockets retain
one 65,535-byte receive buffer per opened family so a protocol-valid reply
payload cannot be truncated; IPv6 jumbograms are unsupported. UDP sockets
instead peek and allocate the exact pending datagram length. DNS messages are
at most 65,535 bytes because TCP DNS uses a 16-bit length prefix. Generated ICMP
errors truncate their quote so the
complete IPv4 error is at most 576 bytes as required by
[RFC 1812 section 4.3.2.3](https://www.rfc-editor.org/rfc/rfc1812.html#section-4.3.2.3),
or the complete IPv6 error is at most the 1,280-byte IPv6 minimum MTU required by
[RFC 4443 section 2.4](https://www.rfc-editor.org/rfc/rfc4443.html#section-2.4).
Malformed inputs that cannot satisfy these structural bounds are dropped rather
than allocated beyond them.

Each Echo remote has 65,536 rewritten sequence values, exactly the complete
16-bit ICMP Echo Sequence Number space. If every value for that remote is still
live, the new request is dropped without disturbing existing sessions; a reply
or the 60-second expiry makes its value reusable.

IPv6 extension processing has no header- or transformation-count cap. [RFC 8200
section 4.1](https://www.rfc-editor.org/rfc/rfc8200.html#section-4.1) requires a
destination to accept and attempt extension headers in any order and number,
except that Hop-by-Hop Options must immediately follow an IPv6 header. Every
successful continuation removes at least one eight-byte extension or Fragment
header from a finite encoded packet. Processing therefore continues until
delivery, pending reassembly or rejection of malformed or unsupported input.

### Lifetimes

| State | Lifetime and source | Expiry behavior |
| --- | --- | --- |
| UDP mapping, contacted remotes and endpoint hop-limit history | 300 seconds idle, the five-minute default recommended by [RFC 4787 section 4.3](https://www.rfc-editor.org/rfc/rfc4787.html#section-4.3); endpoint rows share their IP authorization deadline and have no separate timer | Expiring an IP drops its endpoint evidence. Mapping expiry closes the socket and drops all remaining authorization and endpoint rows; later replies or errors cannot match it. |
| Echo session | 60 seconds from allocation, the minimum ICMP query mapping timer in [RFC 5508 section 3.2](https://www.rfc-editor.org/rfc/rfc5508.html#section-3.2) | The session row is removed; a later reply cannot match it. The shared family socket remains until its owner no longer needs it. |
| IPv4 Identification tuple | Full dataplane session; there is deliberately no timeout or reuse quarantine | The `u16` sequence wraps without denial, and all tuple rows are dropped together at session end. |
| IPv4 incomplete reassembly | 30 seconds, Linux `IP_FRAG_TIME` | The context is discarded and may produce ICMP Time Exceeded only when fragment zero was retained. |
| IPv6 incomplete reassembly | 60 seconds, Linux `IPV6_FRAG_TIMEOUT` | The context is discarded and may produce ICMPv6 Time Exceeded only when fragment zero was retained. |
| TCP established or able to carry data | 7,440 seconds idle, the two-hour-four-minute minimum in [RFC 5382 REQ-5](https://www.rfc-editor.org/rfc/rfc5382.html#section-5) | The expired flow is torn down; any upstream descriptor and lease, plus its buffers, are released. |
| TCP opening or no longer able to carry data | 240 seconds idle, the four-minute transitory minimum in RFC 5382 REQ-5 | The expired flow is torn down; any upstream descriptor and lease, plus its buffers, are released. |
| smoltcp `TIME-WAIT` | The pinned smoltcp 0.13.1 [fixed 10-second close delay](https://github.com/smoltcp-rs/smoltcp/blob/v0.13.1/src/socket/tcp.rs#L297) | Only memory-only client-facing state remains until smoltcp closes it, then the flow record is released; the upstream descriptor and lease ended with its worker. |

TCP half-close preserves byte order and backpressure. After upstream I/O
completes normally, the daemon joins the worker, closes the upstream socket and
immediately releases its descriptor lease, but retains the memory-only
client-facing TCP state until its closing handshake completes. Expiry,
explicit flow teardown or session shutdown may still end it abortively. A reset
is acted on only after the TCP stack accepts it; flow identity is socket handle
plus incarnation so handle reuse cannot tear down a successor.

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
recovery boundary described above. Root mode does not have the
interface-injection limitation.

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
