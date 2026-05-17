# NAT66

NAT66 is VPNHotspot's IPv6 NAT mode. It assigns a deterministic app-owned ULA
prefix to the downstream and proxies selected IPv6 traffic through daemon-owned
sockets. It is not full packet NAT and does not forward arbitrary IPv6 next
headers.

## Runtime Shape

[`nat66::Runtime`](../../mobile/src/main/rust/vpnhotspotd/src/nat66/mod.rs) is
optional per session. If `SessionConfig.ipv6_nat` is absent, NAT66 startup
returns `None` and routing must not install NAT66 interception.

When enabled, startup attempts to create:

- one optional TCP TPROXY listener;
- one optional UDP TPROXY listener;
- an optional router-advertisement task;
- an optional ICMPv6 Echo registration through the process-wide dispatcher.

The TCP and UDP listener ports become routing inputs only when their listeners
start. ICMP Echo support is a capability flag. If one capability fails, the
daemon reports a structured nonfatal and continues with the remaining NAT66
capabilities. If both TCP and UDP fail to start, NAT66 returns no runtime and
the session continues with IPv6 NAT disabled; ICMP alone does not keep NAT66
enabled.

NAT66 shares the session config through an `Arc<Mutex<SessionConfig>>`. Per-flow
or per-packet work should clone a config snapshot at the point where a stable
decision is needed. Long-lived tasks should not keep mutable references to
session config across awaits.

## Ownership Levels

NAT66 state is split by lifetime:

| Lifetime | Owner | State |
| --- | --- | --- |
| Process | `IcmpDispatcher` | NFQUEUE task, ICMP session registrations, shared Echo state, upstream ICMP sockets |
| Session | `nat66::Runtime` | optional TCP/UDP listener ports, optional RA task, ICMP registration, cleanup prefixes |
| Listener | TCP/UDP loops | Accepted TCP connections, UDP association table, reply socket pool |
| Flow | TCP task or UDP association task | Upstream socket, downstream reply path, ICMP error registration where applicable |

The process-wide dispatcher exists because ICMP interception uses one NFQUEUE
number. Sessions register by downstream interface index. The registration is a
weak pointer so stale registrations cannot keep a stopped session alive.

Session stop drops the ICMP registration before prefix withdrawal. Dropping the
registration removes the interface from dispatch, and dropping the session state
removes Echo allocations for that session.

## Network Selection

NAT66 upstream selection uses `primary_network` when the destination matches one
of `primary_routes`; otherwise it uses `fallback_network` when present. Missing
selection usually drops the proxied packet or connection quietly because there
is no daemon-owned upstream path.

Upstream sockets are bound to Android networks with `android_setsocknetwork`.
Reply sockets use the daemon reply mark so responses route through Android's
local-network path before VPN UID rules.

## Routing Contract

Routing owns packet interception. NAT66 owns only the listeners, proxy tasks,
and protocol state behind that interception.

Startup returns `Ipv6NatPorts` to routing:

- TCP and UDP listener ports are optional and each only enables that protocol's
  TPROXY rule;
- `icmp_echo` is true only when the session is registered with the ICMP
  dispatcher;
- missing NAT66 config returns no runtime and no NAT66 routing state.

This is a hard boundary. Routing must not install a NAT66 interception rule
unless the runtime capability exists. Optional TCP, UDP, or ICMP failure must
remove only that protocol's interception from desired routing state.

The mangle path gates downstream traffic through `vpnhotspot_acl`, then through
protocol-specific TPROXY/NFQUEUE handling. Client allow rules return from the
ACL chain; the base ACL rule drops what remains. ICMPv6 control traffic needed
for local link operation returns before protocol interception.

## TCP

TCP interception uses the transparent listener's local address as the original
destination. The TCP runtime:

- marks the inbound socket with the daemon reply mark;
- special-cases DNS to the NAT66 gateway on port 53 and hands it to the DNS TCP
  handler;
- selects an upstream Android network for ordinary unicast destinations;
- connects an upstream TCP socket on that network;
- relays bytes bidirectionally until either side closes or fails.

Connection setup failures caused by the remote path are logged and consumed.
Socket setup failures that indicate daemon or platform state problems are
terminal for that connection task and reported through the normal daemon report
path.

TCP is connection-local. It does not publish separate NAT66 state after the
connection task starts. Closing either side ends the relay; the session runtime
does not track completed TCP connections.

## UDP

UDP interception uses `IPV6_RECVORIGDSTADDR` and hop-limit metadata from the
listener. The UDP runtime owns an association table keyed by client socket and
original destination socket.

For each downstream datagram, the listener:

- handles UDP DNS to the NAT66 gateway through the DNS resolver path;
- ignores special local/link destinations that NAT66 does not own;
- requires downstream hop-limit metadata;
- sends local Time Exceeded when the hop limit expires at the NAT66 boundary;
- selects an upstream Android network;
- reuses or creates a connected upstream UDP socket for that association;
- forwards the datagram with the decremented hop limit when possible.

Each association has one task that owns upstream receives and downstream
responses. The listener owns downstream receives and association creation. The
association task reports activity back to the listener, and idle associations
are cancelled after the NAT66 idle timeout.

ICMP error translation for UDP is association-local. The association registers
only while the connected upstream UDP socket is alive, and it must not revive an
expired association.

The listener is the sole owner of downstream datagram admission. Association
tasks own upstream receives and downstream replies for their connected upstream
socket. Association tasks report activity and close events back to the listener
over an internal channel. If an association closes, the listener removes it only
when the close event matches the current association ID for that key; stale
close events from an older task must not remove a newer association.

The reply socket pool separates DNS replies from user UDP associations. User
associations reserve a reply source while alive so downstream responses can use
the original destination as their source. DNS keeps retained reply sockets by
source/mark because DNS requests are short child tasks rather than entries in
the association table.

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

- one NFQUEUE receive task for downstream Echo Requests;
- a registration map from downstream interface index to weak session state;
- shared Echo mapping state;
- UDP error registrations used by live UDP associations.

A NAT66 session registers ICMP only after proving that downstream send support
is available for its downstream interface and reply mark. Dropping the
registration removes that downstream interface from ICMP dispatch. Dropping the
session removes its Echo state.

ICMP Echo interception is optional. Routing installs ICMP NFQUEUE rules only
when the registration exists. Ordinary local control-plane ICMPv6, neighbour
discovery, router solicitation/advertisement, multicast, link-local, loopback,
and unspecified destinations are outside the Echo proxy ownership boundary.

The NFQUEUE task always gives a verdict for queued packets. Packets with no live
session registration are accepted so stale routing does not blackhole traffic.
Malformed packets and ICMP that NAT66 does not own are dropped or accepted based
on the ownership decision in `nat66/icmp/downstream.rs`.

Routable Echo Requests are copied, then the original queued packet is dropped.
The daemon allocates a rewritten Echo identifier/sequence, records the original
client identity and hop limit, sends a daemon-owned upstream Echo Request on the
selected Android network, and restores the client-visible identifier when the
Echo Reply returns.

Echo upstream sockets are per Android network and shared with UDP ICMP error
translation. They stay alive while that network has Echo allocations or UDP
error registrations. When the last entry for a network expires or is removed,
the upstream socket is cancelled and removed.

ICMP errors are translated only when they map to daemon-owned Echo or UDP state.
Unmapped remote ICMP errors are not guessed into downstream errors.

## Router Advertisements

The RA task owns NAT66 prefix advertisement on the downstream. It sends current
RAs periodically, answers router solicitations, watches downstream IPv6 address
changes, and suppresses or withdraws non-NAT66 downstream prefixes when needed.

The task requires a downstream link-local router address. If the address is not
available, it waits and logs that state instead of inventing a router source.

On NAT66 stop, the runtime waits for the RA task and withdraws the current
gateway prefix. During Clean or replacement cleanup, it may also withdraw older
gateway prefixes recorded during config replacement.

The RA task also watches existing downstream IPv6 prefixes. Non-NAT66 routable
prefixes are temporarily advertised with zero lifetime so clients stop using
them while NAT66 owns the downstream IPv6 mode. Current NAT66 RAs and suppressed
prefix withdrawals use the actual downstream MTU when available, falling back to
the daemon default when MTU lookup fails.

Config replacement can change the deterministic NAT66 gateway. The runtime
records old gateways that need later withdrawal, but only while the process is
alive. Clean still relies on deterministic prefix reconstruction, not this
in-memory list.

## Failure Boundaries

NAT66 startup is best effort across these pieces:

- TCP listener setup failure is nonfatal; NAT66 continues without TCP
  interception when UDP started.
- UDP listener setup failure is nonfatal; NAT66 continues without UDP
  interception when TCP started.
- RA task setup failure is nonfatal; existing NAT66 interception may continue,
  but clients may need other configuration to discover the gateway.
- ICMP registration failure is nonfatal; NAT66 continues without ICMP Echo
  interception.
- ICMP error-queue setup failure is reported, but Echo and UDP may continue
  without that error translation path.
- Runtime packet parse failures, unmapped errors, and ordinary remote failures
  should be dropped or logged without stopping the session.

Session startup treats total NAT66 startup failure as optional at the session
level: if NAT66 produces no TCP or UDP listener, it disables `ipv6_nat` in the
shared config and starts routing without NAT66 interception.

## Cleanup

NAT66 cleanup has two layers:

- session stop cancels runtime tasks and withdraws advertised prefixes;
- routing cleanup removes deterministic routes, addresses, rules, and firewall
  state.

Do not add persisted NAT66 cleanup state. If state can leak after process death,
Clean must be able to reconstruct and delete it from deterministic identifiers,
current interfaces, and the prefix seed.
