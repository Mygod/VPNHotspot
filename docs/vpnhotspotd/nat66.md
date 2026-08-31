# NAT66

NAT66 is VPNHotspot's IPv6 NAT mode. It assigns a deterministic app-owned ULA
prefix to the downstream and proxies selected IPv6 traffic through daemon-owned
sockets. It is not full packet NAT and does not forward arbitrary IPv6 next
headers.

## Runtime Shape

[`root::nat66::Runtime`](../../mobile/src/main/rust/vpnhotspotd/src/root/nat66/mod.rs) is
optional per session. If `SessionConfig.ipv6_nat` is absent, NAT66 startup
returns `None` and routing must not install NAT66 interception.

When enabled, startup attempts to create:

- optional per-MAC TCP TPROXY listeners;
- optional per-MAC UDP TPROXY listeners;
- an optional router-advertisement task;
- an optional ICMPv6 Echo registration through the process-wide dispatcher.

The TCP and UDP listener ports become routing inputs only when their listener,
MAC-scoped routing rules, gateway DNS preludes, local/special destination
exclusions, and base input filter rules are committed. ICMP Echo support is one
session-level capability. If one MAC/protocol capability fails, the daemon
reports a structured nonfatal and continues with the remaining NAT66
capabilities. An empty client set is a deferred state, not a NAT66 failure: the
session may keep `ipv6_nat` configured with no committed NAT66 interception
until a later neighbour snapshot adds a MAC. If clients are present and no TCP
or UDP listener can be committed, the session continues with IPv6 NAT disabled;
ICMP alone does not keep NAT66 enabled.

NAT66 shares the session config through an `Arc<Mutex<SessionConfig>>`. Per-flow
or per-packet work should clone a config snapshot at the point where a stable
decision is needed. Long-lived tasks should not keep mutable references to
session config across awaits.

ICMP registration uses the session routing request connection before routing
takes ownership. The RA task owns separate request and IPv6-address event
connections.

## Ownership Levels

NAT66 state is split by lifetime:

| Lifetime | Owner | State |
| --- | --- | --- |
| Process | `IcmpDispatcher` | NFQUEUE task on queue `30000`, ICMP session registrations, shared Echo state, upstream ICMP sockets |
| Process | `ReplySocketPool` | daemon reply mark, exact UDP reply-source registry, socket leases shared across sessions |
| Session | `root::nat66::Runtime` | committed per-MAC TCP/UDP capabilities, optional RA task, ICMP registration, cleanup prefixes, counter store |
| RA task | `root/nat66/ra.rs` | IPv6-address event and request connections, raw ICMPv6 receive socket, periodic/suppression state |
| Client | per-MAC runtime | TCP/UDP listener ports, stop token, active DNS/NAT66 tasks, source-scoped counters |
| Listener | TCP/UDP loops | Accepted TCP connections, UDP association table, per-listener DNS reply-socket anchor |
| Flow | TCP task, UDP association task, or UDP DNS query task | MAC/downstream accounting context, upstream socket, downstream reply path and reply-socket lease, ICMP error registration where applicable |

The process-wide dispatcher exists because ICMP interception uses one NFQUEUE
number. Sessions register by downstream interface index and committed MAC set.
The registration is a weak pointer so stale registrations cannot keep a stopped
session alive.

Session stop drops the ICMP registration before prefix withdrawal. Dropping the
registration removes the interface from dispatch, and dropping the session state
removes Echo allocations for that session. Removing one MAC cancels only that
MAC's client runtime, including active Echo allocations and UDP error
registrations attributed to that MAC.

## Network Selection

NAT66 upstream selection uses `primary_network` when the destination matches one
of `primary_routes`; otherwise it uses `fallback_network` when present. Missing
selection usually drops the proxied packet or connection quietly because there
is no daemon-owned upstream path.

Upstream sockets are bound to Android networks with `android_setsocknetwork`.
Reply sockets use the daemon reply mark so responses route through Android's
local-network path before VPN UID rules.

NAT66 DNS traffic to the NAT66 gateway reuses the DNS runtime's TCP and UDP
handlers. Host- or network-unreachable DNS TCP response writes are treated as
downstream reachability churn and logged, matching direct DNS TCP handling,
instead of surfacing as `nat66.tcp_connection` nonfatals.

The selected Android network is external state owned by ConnectivityService and
netd. If it disappears between session config publication and upstream socket
setup, `android_setsocknetwork` can fail with `ENONET`. NAT66 logs that
per-flow/per-association setup loss with the selected network and role, then
drops the intercepted connection or datagram without emitting a structured
nonfatal. No daemon-owned socket state is committed in that case, so normal
session stop, replacement, process death, or Clean has no extra cleanup work.

## Routing Contract

Routing owns packet interception. NAT66 owns only the listeners, proxy tasks,
and protocol state behind that interception.

Startup and replacement stage NAT66 capabilities for routing:

- TCP and UDP listener ports are keyed by MAC and protocol, and each enables
  one MAC/protocol listener TPROXY rule for gateway DNS and upstream proxying;
- `icmp_echo` is true only when the session is registered with the ICMP
  dispatcher;
- an empty client set publishes no NAT66 routing capabilities even if a NAT66
  runtime is retained for counters or future clients;
- missing NAT66 config returns no runtime and no NAT66 routing state.

This is a hard boundary. Routing must not install a NAT66 interception rule
unless the runtime capability exists. Optional TCP, UDP, or ICMP failure must
remove only that protocol's interception from desired routing state.

A staged per-MAC TCP or UDP listener becomes committed only if routing also
installs the required session-level local/special exclusions, base input filter
rules, matching MAC-scoped TPROXY rule, and gateway DNS preludes. If routing
fails those rules, the listener is cancelled and the committed session omits
that MAC/protocol capability.

The mangle path returns local-link ICMPv6 control traffic first. Downstream
gateway DNS goes through `vpnhotspot_acl`, then admitted gateway DNS reaches
protocol-specific handling. Other local or special destinations return before
the generic upstream ACL/proxy path. The remaining upstream path gates through
`vpnhotspot_acl`, then applies per-MAC TCP/UDP TPROXY or ICMPv6 NFQUEUE
handling. Client allow rules return from the ACL chain; the base ACL rule drops
what remains. Blocked or unknown MACs never reach daemon-owned DNS, TCP/UDP
upstream TPROXY, or ICMPv6 NFQUEUE handling. TCP and UDP listeners bind to
`::1` and routing uses `--on-ip ::1`, so NAT66 listener ports are internal
TPROXY endpoints. Direct connections to those ports on local or special
downstream destinations do not reach the listener and fall through the base
input reject path.

## TCP

TCP interception uses the transparent listener's local address as the original
destination. Each per-MAC listener carries the MAC/downstream accounting
context for accepted connections. The TCP runtime:

- marks the inbound socket with the daemon reply mark;
- special-cases DNS to the NAT66 gateway on port 53 and hands it to the DNS TCP
  handler;
- selects an upstream Android network for ordinary unicast destinations;
- connects an upstream TCP socket on that network;
- relays bytes bidirectionally, preserving TCP half-close semantics.

The transparent listener requests an `i32::MAX` accept backlog rather than
imposing a Rust connection quota. Linux clamps that request to the runtime
`net.core.somaxconn`; when the kernel queue is full, normal TCP retry/drop
behavior applies and no Rust connection state has been allocated. This is the
same backlog ownership documented by
[`listen(2)`](https://man7.org/linux/man-pages/man2/listen.2.html).

Connection setup failures caused by the remote path are logged and consumed.
When `android_setsocknetwork` reports `ENONET`, the selected upstream network
handle has already disappeared; the connection is logged and dropped because no
upstream socket state was committed. Other socket setup failures that indicate
daemon or platform state problems are terminal for that connection task and
reported with the selected network context through the daemon report path.

TCP is connection-local. It does not publish separate NAT66 state after the
connection task starts. A graceful EOF on one side shuts down only the write half
of the opposite socket and the other direction keeps relaying until it also
closes or an I/O error occurs. Reset, broken-pipe, timeout, host-unreachable, and
network-unreachable errors end only the connection task. The session runtime
does not track completed TCP connections.

TCP relay reports are attributed to the relay leg before they leave the
connection task. Read, write, flush, and shutdown errors use separate contexts
under `nat66.tcp_relay.inbound_to_outbound.*` or
`nat66.tcp_relay.outbound_to_inbound.*` and include the MAC, client,
destination, selected network, role, direction, operation, and relay stage.
Expected connection-close and route-unreachable errors remain log-only;
unexpected relay I/O errors become structured daemon nonfatals with that relay
context preserved.

Each direction uses pinned Tokio 1.53.1's maintained 8,192-byte
[`io::copy` scratch buffer](https://github.com/tokio-rs/tokio/blob/tokio-1.53.1/tokio/src/io/util/mod.rs#L88).
Filling that buffer completes one read/write iteration and the readiness-driven
loop continues, so the value bounds temporary relay storage without truncating,
dropping or refusing stream bytes. Each live association can therefore hold one
such scratch buffer per direction until relay completion or teardown.

TCP byte counters update during relay. NAT66 TCP also increments its sent packet
counter once after a remote upstream socket is successfully opened; that counter
is a connection count, not an IP or TCP segment count.

## UDP

UDP interception uses `IPV6_RECVORIGDSTADDR` and hop-limit metadata from each
per-MAC listener. The UDP runtime owns an association table keyed by MAC, client
socket, and original destination socket.

For each downstream datagram, the listener:

- handles UDP DNS to the NAT66 gateway through the DNS resolver path;
- ignores special local/link destinations that NAT66 does not own;
- requires downstream hop-limit metadata;
- sends local Time Exceeded when the hop limit expires at the NAT66 boundary;
- selects an upstream Android network;
- reuses or creates a connected upstream UDP socket for that association;
- forwards the datagram with the decremented hop limit when possible.

UDP counts one sent unit per upstream datagram and one received unit per
upstream response datagram. NAT66 gateway DNS datagrams are counted by DNS, not
by the `/nat66/udp` source.

Creating a UDP association first selects an Android network, binds the upstream
socket to that network, and connects the socket to the IPv6 destination. `ENONET`
from network selection is treated the same as TCP setup loss. `EHOSTUNREACH` or
`ENETUNREACH` from the connected UDP socket setup means the selected network
accepted the socket but cannot route that destination, for example a fallback
network without a usable IPv6 route. NAT66 logs the failure with MAC, client,
destination, selected network, and role, then drops the datagram without creating
an association. Other UDP setup/connect failures remain structured nonfatals.

Each association has one task that owns upstream receives and downstream
responses. The listener owns downstream receives and association creation. The
association task reports activity back to the listener. Its upstream socket,
ICMP registration and reply-socket lease remain live for 300 seconds after the
last successful downstream send or upstream response. This is RFC 4787 section
4.3 REQ-5's recommended five-minute UDP mapping default; the same requirement
forbids a general timer shorter than two minutes. Expiry cancels and removes the
association, late replies are dropped, and a later downstream datagram creates a
new association.

The listener and each association retain one 65,535-byte receive buffer, the
largest payload length representable by an ordinary IPv6 header. RFC 2675
jumbograms are outside this NAT66 dataplane; standard datagrams need no larger
buffer and are relayed without a daemon-local size quota.

Reply socket leases keep the daemon-wide pool aware of an exact reply-source
bind while an association may send responses through it. Association teardown
releases its lease, but the socket remains registered while another association,
DNS anchor, or DNS query task still holds a lease. This prevents overlapping
per-MAC listeners or sessions from racing a duplicate transparent bind.

ICMP error translation for UDP is association-local and MAC-attributed. The
association registers only while the connected upstream UDP socket is alive, and
it must not revive an expired association.

The listener is the sole owner of downstream datagram admission. Association
tasks own upstream receives and downstream replies for their connected upstream
socket. Association tasks report activity and close events back to the listener
over an internal channel. If an association closes, the listener removes it only
when the close event matches the current association ID for that key; stale
close events from an older task must not remove a newer association.

The control process owns one reply socket pool for every NAT66 UDP listener in
every session. Its reply mark is immutable, and it serializes create-or-reuse
for each exact IPv6 source address and port. Consequently, one bound socket for
that source and mark is shared across MAC listeners and sessions rather than
being recreated in each listener. User associations hold leases while alive.
Each listener also keeps a DNS anchor lease for its gateway source, and each
short-lived DNS query task holds its own lease so listener cancellation cannot
close the socket while a detached query is completing. The last lease removes
the entry and closes the socket.

A reply socket remains a nonblocking IPv6 UDP socket with `SO_REUSEADDR`, the
daemon `SO_MARK`, and `IPV6_TRANSPARENT`, bound to the exact original destination
address and port that must appear as the downstream reply source. The socket is
unconnected and does not enable `IPV6_RECVERR`, so Android's
[UDPv6 error path](https://android.googlesource.com/kernel/common/+/refs/tags/android16-6.12-2025-07_r3/net/ipv6/udp.c#690)
does not place remote ICMP errors in `sk_err` for a later send to consume. Each
downstream send is attempted once. Host- or network-unreachable sends are treated
as client reachability churn and logged. Reply socket acquisition and other
unexpected send failures remain structured nonfatals; neither failure creates a
second exact bind while the original socket is still live.

UDP hop-limit behavior is part of the NAT66 contract. Missing hop-limit
metadata is reported and the datagram is dropped. Expired hop limit produces a
local Time Exceeded from the NAT66 gateway. Forwarded datagrams use the
decremented hop limit when sent upstream.

If a datagram exceeds the upstream socket MTU, UDP sends a Packet Too Big back
downstream when it has enough context. If the connected upstream socket reports
ICMP errors through the error queue, the association's ICMP registration maps
those errors back to the original client/destination.

## ICMPv6

The ICMP dispatcher is process-wide, but registrations are per NAT66 session.
The dispatcher owns:

- one NFQUEUE receive task for downstream Echo Requests on queue `30000`;
- a registration map from downstream interface index to weak session state and
  committed MAC set;
- shared Echo mapping state;
- UDP error registrations used by live UDP associations.

Dropping the last dispatcher handle cancels the upstream and NFQUEUE tasks. Their
token cancels pending waits and is also checked inside continuously ready packet
and error-queue drains.

A NAT66 session registers ICMP only after proving that downstream send support
is available for its downstream interface and reply mark. If the transparent raw
IPv6 bind probe fails with `EADDRNOTAVAIL`, kernels older than Linux 5.11.14
and unparsable `uname.release` values are treated as expected legacy lack of
support and do not emit a structured nonfatal. The session still continues
without ICMP Echo interception. Dropping the registration removes that
downstream interface from ICMP dispatch. Dropping the session removes its Echo
state.

ICMP Echo interception is optional. Routing installs the ICMP NFQUEUE rule only
when the registration exists. Ordinary local control-plane ICMPv6, neighbour
discovery, router solicitation/advertisement, multicast, link-local, loopback,
and unspecified destinations are outside the Echo proxy ownership boundary.

Queue `30000` explicitly uses 1,024 kernel entries, the directly analogous
`NFQNL_QMAX_DEFAULT` for the same NFQUEUE resource in Android's current
[common kernel](https://android.googlesource.com/kernel/common/+/afea13f9ff7137797a2858fc973c226ec93866aa/net/netfilter/nfnetlink_queue.c#51).
It is fail-closed. If all entries await userspace verdicts, Linux drops each new
packet before Rust receives it, increments the kernel queue-drop counter, and
leaves existing entries unchanged; there is therefore no Rust verdict or
per-packet report for that exhaustion. Closing the dispatcher socket unbinds the
queue and the kernel drops entries still awaiting verdicts. Netlink `ENOBUFS`
notifications are enabled so a userspace socket-delivery overrun reaches the
structured queue-receive error path; that notification is distinct from an
NFQUEUE-full drop for which no userspace message exists.

The daemon requests the maximum nfnetlink copy range, but the 16-bit netlink
attribute length includes its four-byte header, so the current kernel can carry
at most 65,531 packet bytes and marks longer packets with `NFQA_CAP_LEN` as
[documented in the implementation](https://android.googlesource.com/kernel/common/+/afea13f9ff7137797a2858fc973c226ec93866aa/net/netfilter/nfnetlink_queue.c#55).
Rust compares the original and retained lengths and drops an explicitly detected
truncation without parsing or reporting the traffic-controlled event. This is a
structural nfnetlink ceiling: it is four bytes below the largest non-jumbo IPv6
packet, and raising the queue's copy-range request cannot remove it.

For every packet Rust does receive, the NFQUEUE task supplies a verdict. Packets
with no live session registration, no committed IPv6 NAT config, missing source
hardware-address metadata, non-six-byte hardware addresses, or a hardware address
outside the committed MAC set are dropped and reported as structured nonfatals.
The dispatcher must not fall back to source IPv6 neighbour lookup. Malformed
packets and ICMP that NAT66 does not own are dropped or accepted based on the
ownership decision in `root/nat66/icmp/downstream.rs`.

Routable Echo Requests are copied, then the original queued packet is dropped.
The daemon allocates a rewritten Echo identifier/sequence, records the original
client MAC, client IPv6 address, and hop limit, sends a daemon-owned upstream
Echo Request on the selected Android network, and restores the client-visible
identifier when the Echo Reply returns. The mapping remains valid for 60 seconds
from allocation, matching RFC 5508 section 3.2 REQ-2's minimum ICMP Query
mapping lifetime. A reply/error, client or session removal releases it earlier;
an expired reply is dropped and a later request allocates a new mapping.
For one `(network, destination, original sequence)` tuple, rewritten identifiers
can use exactly all 65,536 values in ICMPv6's 16-bit Echo Identifier field. If all
are live, the daemon refuses and reports only the new request; existing mappings
remain intact until a reply, error, session/client removal, or the 60-second
expiry releases an identifier.
The upstream raw socket retains one 65,535-byte payload buffer, matching the
ordinary IPv6 Payload Length field; RFC 2675 jumbograms are unsupported.
The NFQUEUE path does not reassemble fragmented downstream Echo Requests, so an
Echo Request whose Fragment header actually fragments the ICMPv6 payload is not
proxied. Atomic fragments continue through the ordinary Echo path.
If the selected network reports host- or network-unreachable during the
upstream Echo send, the daemon logs the route loss, removes the Echo allocation,
and drops the intercepted request without emitting a structured nonfatal.
An initial upstream send failure can represent one stale asynchronous error on
the connected raw socket. The daemon drains the complete error queue, applies any
translated errors/removals, and retries once only when the allocation remains.
That drain is the sole socket-state refresh; a second failure is current path or
socket state, so it is handled normally and the allocation is removed rather
than retried again.

Echo upstream sockets are per Android network and shared with UDP ICMP error
translation. They stay alive while that network has Echo allocations or UDP
error registrations. When the last entry for a network expires or is removed,
the upstream socket is cancelled and removed.

ICMP errors are translated only when they map to daemon-owned Echo or UDP state.
The translated error types are Destination Unreachable, Packet Too Big, Time
Exceeded, and Parameter Problem. Generated downstream ICMP errors preserve the
upstream offender source when that source is meaningful on the downstream link;
link-local upstream offenders are rewritten to the NAT66 gateway. Error quotes
are parsed through complete supported IPv6 extension-header chains. An atomic or
first fragment can be mapped when its Fragment offset is zero and the quote
contains the complete Echo or UDP header; non-initial fragments cannot identify
daemon-owned transport state and are left unmapped. Reverse translation rewrites
the existing quote, including the transport checksum, while preserving its
extension headers and Fragment identification. Every locally generated ICMPv6
error, including Time Exceeded and Packet Too Big, is at most 1,280 bytes: RFC
4443 section 2.4(c)'s complete-packet limit. The fixed eight-byte ICMPv6 header
and required quoted inner headers are retained; quote bytes beyond the remaining
space are omitted, while malformed input too short for required headers is
dropped rather than partially represented.

The upstream error queue correspondingly retains at most 1,232 invoking-packet
bytes: IPv6's 1,280-byte minimum MTU minus the 40-byte outer IPv6 header and
eight-byte ICMPv6 error header. `MSG_TRUNC` still reports the complete queued
length for traffic accounting; a tail beyond 1,232 bytes is discarded because
it cannot fit in the translated downstream error. Expected kernel socket errors
from remote ICMP delivery are consumed after error-queue processing when no
daemon-owned quote can be mapped. Unmapped remote ICMP errors are not guessed
into downstream errors.

ICMPv6 counters count one sent unit per daemon-owned upstream Echo Request and
one received unit per upstream Echo Reply or upstream ICMPv6 error translated
for daemon-owned Echo/UDP state. Locally generated NAT66 control messages do not
inflate upstream counters.

## Router Advertisements

The RA task owns NAT66 prefix advertisement on the downstream. It sends current
RAs periodically, answers router solicitations, watches downstream IPv6 address
changes, and suppresses or withdraws non-NAT66 downstream prefixes when needed.
Periodic multicast intervals are selected uniformly from 300 through 599 seconds,
matching current Android 17
[`RouterAdvertisementDaemon`](https://android.googlesource.com/platform/packages/modules/Connectivity/+/347fbd34b368d19f0d87e908ea101eed3601a731/Tethering/src/android/net/ip/RouterAdvertisementDaemon.java#89).
Startup or changed content can send immediately; after each periodic/current
attempt, a fresh interval is selected. This bounds multicast wakeups rather than
queued advertisements, so reaching the upper interval drops no state or traffic.
Solicitations from a usable source address are answered to that source. A
solicitation from the unspecified address `::` is answered with a
downstream-scoped all-nodes multicast RA to `ff02::1`, because `::` is not a
routable unicast reply target. Receive retains exactly RFC 4861 section 4.1's
fixed eight-byte Router Solicitation header and uses `MSG_TRUNC` to consume the
complete datagram without retaining options. A datagram shorter than eight
bytes, with a nonzero code, or with a different ICMPv6 type is ignored.

The task requires a downstream link-local router address. If the address is not
available, it waits and logs that state instead of inventing a router source.

When the committed client set is empty, the task suppresses current NAT66 RAs.
If it had advertised the NAT66 prefix before the set became empty, it attempts a
zero-lifetime withdrawal and waits for clients to reappear.

Current advertisements use 7,200 seconds for router, prefix valid/preferred, and
RDNSS lifetimes: twelve times Android's 600-second maximum multicast interval,
matching the
[same maintained AOSP policy](https://android.googlesource.com/platform/packages/modules/Connectivity/+/347fbd34b368d19f0d87e908ea101eed3601a731/Tethering/src/android/net/ip/RouterAdvertisementDaemon.java#91)
and allowing loss of several periodic RAs. A zero-lifetime advertisement removes
the named prefix and RDNSS state; it retains the 7,200-second router lifetime only
when the current NAT66 router must remain advertised. If normal
stop/process-death withdrawal does not reach a client, the last accepted current
router/prefix/DNS state expires no later than 7,200 seconds after that
advertisement.

On NAT66 stop, the runtime waits for the RA task and attempts to withdraw the
current gateway prefix through the session request connection. During Clean or
replacement cleanup, it may also withdraw older gateway prefixes recorded during
config replacement. Failure is reported without reconnecting.

The RA task also watches existing downstream IPv6 prefixes. Non-NAT66 routable
prefixes are temporarily advertised with zero lifetime so clients stop using
them while NAT66 owns the downstream IPv6 mode. Suppression records are retained
for 15 seconds and their zero-lifetime advertisements are spaced by
[Android's three-second minimum and five-advertisement policy](https://android.googlesource.com/platform/packages/modules/Connectivity/+/347fbd34b368d19f0d87e908ea101eed3601a731/Tethering/src/android/net/ip/RouterAdvertisementDaemon.java#100),
producing at most five urgent advertisements before the record expires; a later
address event can recreate it. Solicited unicast replies are request-driven and
do not enter that multicast urgent queue. Current NAT66 RAs and suppressed-prefix
withdrawals use the actual downstream MTU when available. If MTU lookup fails,
the error is reported and the advertisement uses 1,280 bytes, IPv6's
protocol-required minimum, rather than assuming Ethernet's 1,500-byte MTU.

Config replacement can change the deterministic NAT66 gateway. The runtime
records old gateways that need later withdrawal, but only while the process is
alive. Clean still relies on deterministic prefix reconstruction, not this
in-memory list.

## Failure Boundaries

NAT66 startup is best effort across these pieces:

- Per-MAC TCP listener setup or routing failure is nonfatal; NAT66 continues
  without that MAC/protocol interception when other capabilities started.
- Per-MAC UDP listener setup or routing failure is nonfatal; NAT66 continues
  without that MAC/protocol interception when other capabilities started.
- RA task setup failure is nonfatal; existing NAT66 interception may continue,
  but clients may need other configuration to discover the gateway.
- RA transmit `ENOBUFS` is downstream link backpressure, such as a full
  USB/NCM tether transmit queue, and is logged to stderr instead of reported as
  a structured nonfatal. Other RA send failures remain structured nonfatals
  because they can indicate socket, privilege, or platform state problems.
- Current or solicited RA send `EADDRNOTAVAIL` means the downstream link-local
  router address disappeared between lookup and socket setup or transmission,
  such as during tethering teardown. It is logged as a skipped advertisement
  rather than reported as a structured nonfatal.
- RA withdrawal socket bind `EADDRNOTAVAIL` means the downstream link-local
  router address disappeared before cleanup could send a zero-lifetime RA. It
  is logged as skipped cleanup rather than reported as a structured nonfatal.
- ICMP registration failure is nonfatal; NAT66 continues without ICMP Echo
  interception. The known transparent raw IPv6 bind `EADDRNOTAVAIL` failure is
  reported only when `uname.release` parses as Linux 5.11.14 or newer.
- Unattributable ICMP NFQUEUE packets are dropped and reported as nonfatal
  background state.
- ICMP error-queue setup failure is reported, but Echo and UDP may continue
  without that error translation path.
- Runtime packet parse failures, unmapped errors, and ordinary remote failures
  should be dropped or logged without stopping the session.

Session startup treats total NAT66 startup failure as optional at the session
level: if clients are present and NAT66 commits no TCP or UDP listener, it
disables `ipv6_nat` in the shared config and starts routing without NAT66
interception. If there are no clients yet, NAT66 remains eligible for a later
replacement instead of being treated as failed.

## Cleanup

NAT66 cleanup has two layers:

- session stop cancels runtime tasks and attempts to withdraw advertised
  prefixes;
- routing cleanup removes deterministic routes, addresses, rules, and firewall
  state.

Per-MAC runtime removal cancels that MAC's TCP tasks, UDP associations, Echo
allocations, and UDP error registrations, then preserves final source counters
long enough for one structured traffic-counter read.

Reply sockets are process-owned file descriptors rather than routing or
firewall mutations. Session stop releases listener anchors and association
leases. A retained listener swaps its anchor on the first DNS query for a new
gateway after reconfiguration, while overlapping detached tasks keep their
sockets live until they finish. The last lease closes each socket, and process
death closes any remaining descriptors. Normal stop and Clean require no
additional kernel-state cleanup for the reply socket pool.

Do not add persisted NAT66 cleanup state. If state can leak after process death,
Clean must be able to reconstruct and delete it from deterministic identifiers,
current interfaces, and the prefix seed.
