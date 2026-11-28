# Routing

Routing code owns root-side system mutations for a session. It consumes the
ports and capabilities produced by DNS and NAT66 startup, then turns
`SessionConfig` into concrete kernel, netfilter, and netd state.

This document is a mutation catalog. Every route, rule, address, firewall, and
`ndc` mutation made by `routing.rs` and `routing/` should be listed here.

## Mutation Model

[`routing.rs`](../../mobile/src/main/rust/vpnhotspotd/src/routing.rs)
represents session routing state as `RoutingMutation` values:

| Mutation | External apply | Session rollback |
| --- | --- | --- |
| `EnsureIptablesChain` | `iptables-restore`/`ip6tables-restore` `-N <chain>`; after a nonzero exit, verify the chain with `-L <chain> -n` | no-op; chains are scaffold state |
| `Iptables` | `iptables-restore`/`ip6tables-restore` `-I <chain> ...` | delete the same rule with `-D <chain> ...` |
| `IptablesGroup` | one `iptables-restore`/`ip6tables-restore` commit with an `-I <chain> ...` line per rule, so the group lands at the chain head in its listed order | delete each rule bottom-up, stopping at the first failure |
| `LocalAddressReturn` | `ip6tables-restore` `-I` of the filter return, then the mangle return; the filter return is deleted again if the mangle insert fails | delete the mangle return, then the filter return |
| `IpForward` | `ndc ipfwd enable vpnhotspot_<downstream>`, or `/proc/sys/net/ipv4/ip_forward = 1` fallback | `ndc ipfwd disable vpnhotspot_<downstream>` only |
| `Ip` | rtnetlink rule, route, or address replace | rtnetlink delete for the same rule, route, or address |
| `NetdNat` | `ndc nat enable <downstream> <upstream> 0` | no-op; netd has no app-owned token |

`Runtime::reconcile` deletes applied mutations that are no longer desired, then
applies new desired mutations one at a time. Apply failures are structured
nonfatal reports; successful mutations stay applied and are not rolled back only
because a later best-effort mutation failed. A failed desired mutation is not
recorded in `applied`, so a later reconcile can try it again.

Creating an existing iptables chain normally exits nonzero. That result is only
treated as success after the read-only list command verifies that the chain is
present. If creation and verification both fail, the verification stderr is
reported as a structured ensure failure; reconciliation remains best effort and
can retry the ensure later.

The `applied` list is only the current process rollback list. It is not
persisted and is not a Clean source of truth.

`routing::Runtime` owns one rtnetlink request connection for the session
lifetime. Startup transfers the connection used for downstream discovery;
replacement and normal stop reuse it. Request failures are reported and later
operations remain best effort; the runtime does not reconnect. A session that
starts with `SessionConfig.ipv6_nat` also owns a multicast-only IPv6-address
event connection for its
[phone address returns](#nat66-phone-address-returns).

## Session Desired Mutations

[`routing/desired.rs`](../../mobile/src/main/rust/vpnhotspotd/src/routing/desired.rs)
builds this desired state. The order below follows the producer order.

### IP Forwarding

Condition: `SessionConfig.ip_forward`.

External mutation:

- `ndc ipfwd enable vpnhotspot_<downstream>`

Fallback external mutation:

- if `ndc ipfwd enable` fails, write `1` to
  `/proc/sys/net/ipv4/ip_forward`

Rollback:

- `ndc ipfwd disable vpnhotspot_<downstream>`

The sysctl fallback is not reset by session rollback or Clean. It is a
persistent kernel-state fallback used only after the named `ndc ipfwd` enable
path fails.

### IPv4 DNS Redirect

Condition: the owned nat chain and PREROUTING jump are present for a started
session with downstream IPv4. Per-MAC redirect rules are present per committed
client MAC and protocol when DNS startup produced that MAC/protocol listener
port and routing can also install the matching direct-port guard. DNS listener
ports are daemon-owned sockets bound to `0.0.0.0`; the downstream IPv4 address is
owned by routing and appears only in packet matching and conntrack validation.

External mutations:

- ensure `iptables -t nat` chain `vpnhotspot_dns_nat`
- `iptables -t nat -I PREROUTING -j vpnhotspot_dns_nat`
- `iptables -t nat -I vpnhotspot_dns_nat -i <downstream> -p tcp -m mac --mac-source <mac> -d <downstream-ipv4> --dport 53 -j DNAT --to-destination :<dns-tcp-port-for-mac>`
- `iptables -t nat -I vpnhotspot_dns_nat -i <downstream> -p udp -m mac --mac-source <mac> -d <downstream-ipv4> --dport 53 -j DNAT --to-destination :<dns-udp-port-for-mac>`
- `iptables -t filter -I vpnhotspot_dns_input -i <downstream> -p tcp -d <downstream-ipv4> --dport <dns-tcp-port-for-mac> -m conntrack --ctorigdst <downstream-ipv4> --ctorigdstport 53 -j RETURN`
- `iptables -t filter -I vpnhotspot_dns_input -p tcp --dport <dns-tcp-port-for-mac> -j REJECT --reject-with tcp-reset`
- `iptables -t filter -I vpnhotspot_dns_input -i <downstream> -p udp -d <downstream-ipv4> --dport <dns-udp-port-for-mac> -m conntrack --ctorigdst <downstream-ipv4> --ctorigdstport 53 -j RETURN`
- `iptables -t filter -I vpnhotspot_dns_input -p udp --dport <dns-udp-port-for-mac> -j REJECT --reject-with icmp-port-unreachable`

Effective order for a listener port is the conntrack original-destination
`RETURN` before the direct-port `REJECT`. The guard makes the ephemeral listener
port reachable only as the post-DNAT target of a packet originally addressed to
the downstream gateway on port 53. Because listeners are wildcard-bound, the
direct-port `REJECT` is intentionally not scoped to the downstream interface or
gateway address. If routing cannot install or validate the conntrack
original-destination guard, it must omit that MAC/protocol DNS capability
instead of exposing the listener port directly.

The guard lines above are the required effective order. Because iptables `-I`
inserts at the head by default, implementation must either insert with explicit
positions or apply paired guard mutations in the reverse order that produces the
effective chain order.

Rollback:

- delete the PREROUTING jump and the same MAC/protocol redirect and guard rules.
- no session rollback for the `vpnhotspot_dns_nat` chain itself.

Routing only redirects packets. MAC ownership and DNS upstream selection belong
to [`dns.rs`](../../mobile/src/main/rust/vpnhotspotd/src/dns.rs). A DNS
listener is not committed unless both the listener and matching MAC redirect
and direct-port guard rules exist.

### IPv4 Gateway DNS Denial

Condition: always present for a started session with downstream IPv4.

External mutations:

- ensure `iptables -t filter` chain `vpnhotspot_dns_input`
- `iptables -t filter -I INPUT -j vpnhotspot_dns_input`
- `iptables -t filter -I vpnhotspot_dns_input -i <downstream> -p tcp -d <downstream-ipv4> --dport 53 -j REJECT --reject-with tcp-reset`
- `iptables -t filter -I vpnhotspot_dns_input -i <downstream> -p udp -d <downstream-ipv4> --dport 53 -j REJECT --reject-with icmp-port-unreachable`

Rollback:

- delete the INPUT jump and downstream reject rules.
- no session rollback for the chain itself.

Clean:

- delete the INPUT jump, then flush and delete `vpnhotspot_dns_input`.
- delete the PREROUTING jump, then flush and delete `vpnhotspot_dns_nat`.

Allowed DNS packets have already been DNATed to per-MAC daemon listener ports in
`vpnhotspot_dns_nat`, so these rules catch blocked clients and missing DNS
capability cases that remain addressed to the gateway on port 53. They do not
block manually configured external DNS except through the normal upstream
admission rules.

### IPv4 Downstream Block Rule

Condition: always present for a started session.

External mutation:

- replace IPv4 policy rule: `iif <downstream> priority <upstream-disable-priority> unreachable`

Rollback:

- delete IPv4 policy rule at `<upstream-disable-priority>`.

This prevents downstream traffic from escaping through system-owned fallback
routing when VPNHotspot has not allowed a matching upstream path.

### IPv4 Forwarding Chains

Condition: always present for a started session.

External mutations:

- ensure `iptables -t filter` chain `vpnhotspot_acl`
- ensure `iptables -t filter` chain `vpnhotspot_stats`

Session rollback:

- none for the chains themselves.

Clean:

- flush and delete these chains after removing base jumps.

### IPv4 Forwarding Rules

Condition: always present for a started session.

External mutations:

- `iptables -t filter -I FORWARD -j vpnhotspot_acl`
- `iptables -t filter -I vpnhotspot_acl -i <downstream> ! -o <downstream> -j REJECT`
- `iptables -t filter -I vpnhotspot_acl -o <downstream> -m state --state ESTABLISHED,RELATED -j ACCEPT`
- `iptables -t filter -I vpnhotspot_acl -o <downstream> -m state --state ESTABLISHED,RELATED -j vpnhotspot_stats`

Rollback:

- delete the same rules.

Client-specific allow and stats rules are inserted into these chains from the
client snapshot, described below.

### Simple IPv4 Masquerade

Condition: `SessionConfig.masquerade == MASQUERADE_MODE_SIMPLE`.

External mutations:

- ensure `iptables -t nat` chain `vpnhotspot_masquerade`
- `iptables -t nat -I POSTROUTING -j vpnhotspot_masquerade`

For each unique resolved upstream interface:

- `iptables -t nat -I vpnhotspot_masquerade -s <downstream-ipv4-subnet> -o <upstream> -j MASQUERADE`

Rollback:

- delete the jump and upstream MASQUERADE rules.
- no session rollback for the chain itself.

Clean:

- delete the POSTROUTING jump, then flush and delete `vpnhotspot_masquerade`.

### Upstream Policy Rules

Condition: for each unique upstream interface in
`primary_upstream_interfaces` and `fallback_upstream_interfaces`.

External mutations:

- primary upstream: replace IPv4 policy rule
  `iif <downstream> priority <primary-priority> lookup <1000 + upstream-ifindex>`
- fallback upstream: replace IPv4 policy rule
  `iif <downstream> priority <fallback-priority> lookup <1000 + upstream-ifindex>`

Rollback:

- delete the corresponding IPv4 policy rule by full rule shape.

Missing upstream links are skipped because upstream snapshots can race interface
churn. Other netlink lookup errors are reported as structured nonfatals and
that upstream interface is skipped for the current best-effort reconcile.

### Netd Masquerade

Condition: `SessionConfig.masquerade == MASQUERADE_MODE_NETD`, for each unique
resolved upstream interface.

External mutation:

- `ndc nat enable <downstream> <upstream> 0`

Rollback:

- none.

Netd NAT/forwarding state is globally keyed by interface pair without an
app-owned token. Disabling it during session rollback could tear down
platform-owned state, so this daemon intentionally does not issue
`ndc nat disable`.

### IPv6 Block Mode

Condition: `SessionConfig.ipv6_block`.

External mutations:

- ensure `ip6tables -t filter` chain `vpnhotspot_filter`
- `ip6tables -t filter -I INPUT -j vpnhotspot_filter`
- `ip6tables -t filter -I FORWARD -j vpnhotspot_filter`
- `ip6tables -t filter -I OUTPUT -j vpnhotspot_filter`
- `ip6tables -t filter -I vpnhotspot_filter -i <downstream> -j REJECT`
- `ip6tables -t filter -I vpnhotspot_filter -o <downstream> -j REJECT`

Rollback:

- delete the five inserted rules.
- no session rollback for the chain itself.

Clean:

- delete the base jumps, then flush and delete `vpnhotspot_filter`.

### NAT66 Routes, Address, And Policy Rule

Condition: `SessionConfig.ipv6_nat != null` and NAT66 startup committed at
least one TCP or UDP runtime capability.

External mutations:

- replace IPv6 unicast route in table 97:
  `<nat66-prefix> dev <downstream> table local_network`
- replace IPv6 unicast route in the downstream's netd interface table:
  `<nat66-prefix> dev <downstream> table <1000 + downstream-ifindex>`
- replace IPv6 address on the downstream:
  `<nat66-gateway>/<prefix-len> dev <downstream>`
- replace IPv6 local route in table 900:
  `local ::/0 dev lo table 900`
- NAT66 daemon policy-rule priority is `20600` on API 31+ and `17600` on
  API 29..30.
- in protocol-rule mode, replace one IPv6 policy rule per active NAT66
  listener protocol:
  - TCP listener present:
    `iif <downstream> priority <nat66-daemon-priority> ipproto tcp lookup 900`
  - UDP listener present:
    `iif <downstream> priority <nat66-daemon-priority> ipproto udp lookup 900`
- in fwmark fallback mode, replace one IPv6 policy rule:
  `iif <downstream> priority <nat66-daemon-priority> fwmark 0x10000000/0x10000000 lookup 900`

Rollback:

- delete the same routes, address, local route, and policy rule.

Clean:

- flush IPv6 routes from table 900.
- reconstruct `<nat66-prefix>` and `<nat66-gateway>` for every current
  interface from the Clean prefix seed, then delete the gateway address, the
  table 97 route, and the route in table `<1000 + current-ifindex>` for each
  interface.

Clean never flushes table 97 because it is Android's shared `local_network`
table, nor an interface table because netd owns it.

The interface-table route exists for downstreams that netd keeps in their own
network. On Android 15+, `IpServer` can register a local network agent for each
tethering or Wi-Fi P2P group-owner downstream instead of adding it to
`LOCAL_NET_ID` 99. Whether it does depends on the platform release, not only on
the Tethering module: on 25Q4+ releases the agent is on unless a kill switch
disables it, and on earlier Android 15+ releases only a beta flag enables it.
ConnectivityService creates the agent's network as `PHYSICAL_LOCAL`. For such
a network, netd resolves the interface table as ifindex + 1000 when the
downstream joins, marks packets arriving on it with the agent's netId, and
points the network's explicit `fwmark <netId>/0x1ffff` rule at that table
rather than table 97. A locally delivered TCP connection's accepted socket
inherits the incoming mark, and kernel replies such as RSTs and ICMPv6 errors
reflect it, so their replies to the NAT66 prefix need a route in that table.
Only replies carrying the agent's netId need it. Android kernels may already
provide one: netd sets the downstream's `accept_ra_rt_table` to -1000, and
Android's per-device-table kernel change can then put the prefix route of the
gateway address into the same interface table. In that case this route is
redundant but harmless; the device test shows which applies. The agent network also gets an implicit
`from all fwmark 0x0/0x10000` local-network rule for the interface table, but
netd installs the table-97 rule at that priority when it starts, and the kernel
places a new rule after existing rules of equal priority, so an unmarked reply
still finds the NAT66 prefix in table 97 first. In
`LOCAL_NET_ID` mode netd maps the downstream to table 97 and no rule selects its
interface table, so the route there is inert. Routing installs it
unconditionally rather than detecting netd's network membership, which netd
does not expose and which would race the agent's registration. Daemon reply
sockets use the `LOCAL_NET_ID` reply mark, which keeps selecting table 97 in
either mode.

The table number comes from the downstream's ifindex at reconcile time, as for
upstream tables. A missing downstream link skips the route; another lookup
failure is a structured nonfatal `routing.resolve_downstream_index` and skips it
for that reconcile. The route is not a commit requirement: without it, and
without a kernel prefix route in that table, only replies that select the agent
network's table are lost. When the downstream
leaves its agent network, netd flushes the interface table, so a later
rollback delete finds the route missing, which is expected.

AOSP sources:

- local network agent:
  https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/Tethering/src/android/net/ip/IpServer.java#408
  and
  https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/Tethering/src/android/net/ip/IpServer.java#1226
- `PHYSICAL_LOCAL` network creation:
  https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#6374
- interface table resolution:
  https://android.googlesource.com/platform/system/netd/+/android-17.0.0_r1/server/RouteController.cpp#146
- incoming mark, explicit rule, and local-network rule for the interface table:
  https://android.googlesource.com/platform/system/netd/+/android-17.0.0_r1/server/RouteController.cpp#903
- `LOCAL_NET_ID` interfaces mapped to table 97:
  https://android.googlesource.com/platform/system/netd/+/android-17.0.0_r1/server/RouteController.cpp#1284
- interface table flush on removal:
  https://android.googlesource.com/platform/system/netd/+/android-17.0.0_r1/server/RouteController.cpp#1320
- `accept_ra_rt_table` set to -1000 on all interfaces:
  https://android.googlesource.com/platform/system/netd/+/android-17.0.0_r1/server/InterfaceController.cpp#247

Before routing the first candidate NAT66 TCP/UDP listener ports, routing probes
kernel `FRA_IP_PROTO` support through rtnetlink once per daemon process and
reuses the cached result for later NAT66 sessions. NAT66-enabled sessions with
no candidate TCP/UDP listener ports do not probe yet because there is no
listener interception rule to choose. The probe adds a temporary
detached-interface rule at `<nat66-daemon-priority>`:
`iif vpnhs_probe0 priority <nat66-daemon-priority> ipproto tcp lookup 900`. Routing
then dumps IPv6 rules and requires the echoed rule to include `ipproto tcp`.
Normal probe cleanup deletes at most one rule: the exact protocol form when the
dump echoed it, or a selector without `ipproto` otherwise. The protocol-less
selector is also the best-effort rollback when the add or dump outcome is
uncertain. Missing state is accepted; other deletion failures are reported.
Repeated deletion of stale or duplicate rules is reserved for Clean. This
detached interface is intentional: kernels without `FRA_IP_PROTO` can silently
ignore the unknown attribute and accept a bare `iif ... lookup 900` rule, so
probing with the real downstream would create a transient or leaked
traffic-affecting rule.

If the probe fails, routing uses fwmark fallback mode. When `uname.release`
parses as Linux 4.17 or newer, the fallback is also reported as a structured
nonfatal warning tied to the start-session call because upstream Linux has
supported `FRA_IP_PROTO` since 4.17. Older or unparsable releases use fallback
without that warning.

### NAT66 Firewall Chains

Condition: `SessionConfig.ipv6_nat != null` and NAT66 startup committed at
least one TCP or UDP runtime capability.

External mutations:

- ensure `ip6tables -t filter` chain `vpnhotspot_v6_input`
- ensure `ip6tables -t filter` chain `vpnhotspot_v6_forward`
- ensure `ip6tables -t filter` chain `vpnhotspot_v6_output`
- ensure `ip6tables -t mangle` chain `vpnhotspot_acl`
- ensure `ip6tables -t mangle` chain `vpnhotspot_v6_acl_gate`
- ensure `ip6tables -t mangle` chain `vpnhotspot_v6_protocols`
- ensure `ip6tables -t mangle` chain `vpnhotspot_v6_tproxy`

Session rollback:

- none for the chains themselves.

Clean:

- delete base jumps, then flush and delete these chains.

### NAT66 Filter Rules

Condition: `SessionConfig.ipv6_nat != null` and NAT66 startup committed at
least one TCP or UDP runtime capability.

External mutations:

- `ip6tables -t filter -I vpnhotspot_v6_input -i <downstream> -j REJECT`
- `ip6tables -t filter -I vpnhotspot_v6_input -i <downstream> -m socket --transparent -j ACCEPT`
- `ip6tables -t filter -I vpnhotspot_v6_input -i <downstream> -p icmpv6 -j ACCEPT`
- `ip6tables -t filter -I vpnhotspot_v6_forward -o <downstream> -j REJECT`
- `ip6tables -t filter -I vpnhotspot_v6_forward -i <downstream> -j REJECT`
- `ip6tables -t filter -I vpnhotspot_v6_output -o <downstream> -p icmpv6 --icmpv6-type 134 -j REJECT`
- `ip6tables -t filter -I vpnhotspot_v6_output -o <downstream> -p icmpv6 --icmpv6-type 134 -m mark --mark 0x00030063/0x0003ffff -j ACCEPT`
- gateway input group, inserted by one `ip6tables-restore` commit in this
  order:
  - `-I vpnhotspot_v6_input -i <downstream> -d <nat66-gateway>/128 -j RETURN`
  - `-I vpnhotspot_v6_input -i <downstream> -d <nat66-gateway>/128 -p udp --dport 53 -j REJECT`
  - `-I vpnhotspot_v6_input -i <downstream> -d <nat66-gateway>/128 -p tcp --dport 53 -j REJECT --reject-with tcp-reset`
  - `-I vpnhotspot_v6_input -i <downstream> -d <nat66-gateway>/128 -m socket --transparent -j ACCEPT`

Rollback:

- delete the same rules. The gateway input group is deleted bottom-up, the
  `RETURN` first and the transparent-socket `ACCEPT` last, and stays applied
  until every deletion succeeds; a retry repeats from the first remaining rule.

These rules reject non-daemon IPv6 forwarding while allowing daemon-owned
transparent sockets and daemon-marked router advertisements. Because rules are
inserted with `-I`, rule order must be reviewed together with insertion order
when changing this list. The per-address `vpnhotspot_v6_input` returns in
[NAT66 Phone Address Returns](#nat66-phone-address-returns) sit above the
downstream `REJECT`, so input to the phone's own addresses continues to the
platform's `INPUT` rules instead.

The gateway input group gives the NAT66 gateway the reachability IPv4 gives the
downstream address: phone services listening on it, or on the wildcard address,
can be reached from the downstream at `<nat66-gateway>`, as at
`<downstream-ipv4>`. The mangle path already returns the whole gateway prefix
before the ACL gate, so this includes blocked MACs, as for IPv4; only gateway
DNS on port 53 passes the ACL gate.

Port 53 stays reserved for gateway DNS interception, as the IPv4 gateway DNS
denial keeps it. The gateway DNS preludes TPROXY admitted DNS to the daemon
listener in mangle `PREROUTING`, before any filter rule, and the group's own
transparent-socket `ACCEPT` accepts it. DNS that reaches the gateway without
interception, for example over UDP when only TCP listeners committed, finds no
transparent socket and is rejected by the port 53 `REJECT`s, as the downstream
`REJECT` refused it before the gateway was opened. Because one restore commit
inserts the whole group at the chain head in the listed order, the
transparent-socket `ACCEPT` sits above the `REJECT`s whenever the group is
installed, including a retry or a gateway change after the rest of the session
firewall. The effective session order is phone address returns, then the group
(gateway transparent-socket `ACCEPT`, TCP and UDP port 53 `REJECT`s, gateway
`RETURN`), then ICMPv6 `ACCEPT`, transparent-socket `ACCEPT`, and the downstream
`REJECT`. ICMPv6 to the gateway therefore continues to the platform's `INPUT`
rules, as ICMPv6 to the phone's other addresses does.

NAT66 listener ports stay unreachable at the gateway because the listeners bind
`::1`. The group is not a commit requirement; without it, non-DNS input to the
gateway is refused as before. If its insert fails, nothing of it is installed.

Process death leaves these rules in place until Clean, which flushes and deletes
the NAT66 filter chains.

The mark value is `DAEMON_REPLY_MARK/DAEMON_REPLY_MARK_MASK`.

### NAT66 ACL Gate

Condition: `SessionConfig.ipv6_nat != null` and NAT66 startup committed at
least one TCP or UDP runtime capability.

External mutation:

- `ip6tables -t mangle -I vpnhotspot_v6_acl_gate -i <downstream> -j vpnhotspot_acl`

Rollback:

- delete the same rule.

The process-wide NAT66 base rule in `vpnhotspot_acl` drops packets that do not
return from the ACL chain through a client allow rule.

### NAT66 ICMP Echo NFQUEUE

Condition: `SessionConfig.ipv6_nat != null` and NAT66 runtime reported
`icmp_echo = true`.

External mutation:

- `ip6tables -t mangle -I vpnhotspot_v6_protocols -i <downstream> -p icmpv6 --icmpv6-type echo-request ! -d <nat66-gateway> -j NFQUEUE --queue-num 30000`

Rollback:

- delete the same rule.

Routing must omit this rule when ICMP registration failed. Packets must not be
queued unless the process-wide ICMP dispatcher has a live session registration
for the downstream interface. The rule is session-level rather than per-MAC
because NAT66 ACL admission has already run before protocol interception.

ICMP Echo interception intentionally uses NFQUEUE rather than TCP/UDP-style
TPROXY. ICMP has no destination port for a transparent listener, and the daemon
must drop the original queued Echo Request after copying it so it cannot continue
through another forwarding path alongside the daemon's translated probe.

The dispatcher attributes queued packets from `NFQA_HWADDR`. Missing
hardware-address metadata, non-six-byte hardware addresses, and MACs outside
the committed client set are dropped and reported as structured nonfatals. The
dispatcher must not fall back to source IPv6 neighbour lookup.

### NAT66 TCP/UDP TPROXY

Condition: `SessionConfig.ipv6_nat != null` and NAT66 startup committed at
least one TCP or UDP runtime capability. Gateway DNS preludes and local/special
destination returns are session-level rules. Listener TPROXY rules are per
committed client MAC/protocol listener port. NAT66 gateway DNS and upstream
proxying share the same per-MAC listener; the daemon distinguishes gateway DNS
by the original destination. Local or special destinations do not enter the
daemon-owned upstream proxy path: the fixed returns below cover the session's
gateway prefix, link-local, multicast, unspecified, and loopback destinations,
and [NAT66 Phone Address Returns](#nat66-phone-address-returns) cover every
other address the phone currently has.

External mutations:

- gateway DNS TCP ACL/protocol prelude:
  - `ip6tables -t mangle -I vpnhotspot_v6_tproxy -i <downstream> -p tcp -d <nat66-gateway> --dport 53 -j vpnhotspot_v6_acl_gate`
  - `ip6tables -t mangle -I vpnhotspot_v6_tproxy -i <downstream> -p tcp -d <nat66-gateway> --dport 53 -j vpnhotspot_v6_protocols`
- gateway DNS UDP ACL/protocol prelude:
  - `ip6tables -t mangle -I vpnhotspot_v6_tproxy -i <downstream> -p udp -d <nat66-gateway> --dport 53 -j vpnhotspot_v6_acl_gate`
  - `ip6tables -t mangle -I vpnhotspot_v6_tproxy -i <downstream> -p udp -d <nat66-gateway> --dport 53 -j vpnhotspot_v6_protocols`
- local/special destination returns before the generic upstream ACL/proxy path:
  - `ip6tables -t mangle -I vpnhotspot_v6_tproxy -i <downstream> -d <nat66-prefix> -j RETURN`
  - `ip6tables -t mangle -I vpnhotspot_v6_tproxy -i <downstream> -d fe80::/10 -j RETURN`
  - `ip6tables -t mangle -I vpnhotspot_v6_tproxy -i <downstream> -d ff00::/8 -j RETURN`
  - `ip6tables -t mangle -I vpnhotspot_v6_tproxy -i <downstream> -d ::/127 -j RETURN`
- TCP listener:
  - `ip6tables -t mangle -I vpnhotspot_v6_protocols -i <downstream> -p tcp -m mac --mac-source <mac> -j TPROXY --on-ip ::1 --on-port <nat66-tcp-port-for-mac>`
- UDP listener:
  - `ip6tables -t mangle -I vpnhotspot_v6_protocols -i <downstream> -p udp -m mac --mac-source <mac> -j TPROXY --on-ip ::1 --on-port <nat66-udp-port-for-mac>`

Effective mangle order is gateway DNS `:53` ACL gate, gateway DNS `:53`
protocol interception, local/special destination returns and phone address
returns, generic upstream ACL gate, then broad per-MAC listener TPROXY for the
remaining TCP or UDP traffic.
Gateway DNS enters the same listener as ordinary upstream traffic, but the
daemon routes it to DNS handling from the original destination. This keeps local
traffic outside NAT66 admission while still applying the allow/block decision to
daemon-owned DNS and upstream proxying.

The command list above is the required effective order. Because iptables `-I`
inserts at the head by default, implementation must either insert with explicit
positions or apply rule mutations in the reverse order that produces the
effective chain order.

In fwmark fallback mode, the TPROXY rules also append
`--tproxy-mark 0x10000000/0x10000000`.

Rollback:

- delete the same rules.

The TCP and UDP rules use `--on-ip ::1`, and the listeners bind to `::1`.
Listener ports are internal TPROXY endpoints rather than downstream-reachable
service ports. Direct downstream local/special traffic to those ports does not
match the listener's `::1` bind. Traffic to a special destination then falls
through the base input reject path; traffic to the gateway or another phone
address passes its input return and reaches no daemon listener.

A per-MAC TCP or UDP listener is not committed unless the daemon listener,
required session-level local/special exclusions, base input filter rules,
gateway DNS preludes, and matching MAC-scoped listener TPROXY rule all exist.
If routing fails the rules required for that MAC/protocol, the staged listener
is cancelled and that MAC/protocol capability is omitted from the committed
session. Phone address returns are not commit requirements; a missing pair
only leaves that address on the proxied path.

### NAT66 Phone Address Returns

Condition: the NAT66 session firewall above is present, and the session
follows the phone's IPv6 addresses. A session follows them when it starts with
`SessionConfig.ipv6_nat` and its address subscription succeeds. Replacement
that clears `ipv6_nat` stops following.

External mutations, for each followed address outside the session's
`<nat66-prefix>`, filter rule first:

- `ip6tables -t filter -I vpnhotspot_v6_input -i <downstream> -d <phone-address>/128 -j RETURN`
- `ip6tables -t mangle -I vpnhotspot_v6_tproxy -i <downstream> -d <phone-address>/128 -j RETURN`

Each address's two rules are one `LocalAddressReturn` mutation, so the mangle
return never stands without the filter return. Applying inserts the filter
return, then the mangle return; if the mangle insert fails, it deletes the
filter return again and leaves the pair unapplied. Deleting removes the mangle
return, then the filter return, and keeps the pair in the applied list until
both deletions succeed.

Followed addresses come from an `AF_INET6` `RTM_GETADDR` dump across all
interfaces, parsed by
[`shared/local_ipv6.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/local_ipv6.rs).
Each distinct address counts once, taking `IFA_LOCAL` over `IFA_ADDRESS` for
point-to-point addresses. The parser leaves out:

- addresses still tentative without being optimistic, and DAD-failed
  addresses, because the kernel does not deliver to them yet and the address
  may belong to another node on its link;
- addresses inside `fe80::/10`, `ff00::/8`, or `::/127`, which the fixed
  local/special returns above already cover. The session's own gateway prefix
  is left out when the rules are built, for the same reason.

Effect: a downstream packet addressed to the phone's own address leaves
`vpnhotspot_v6_tproxy` before the ACL gate and TPROXY, and leaves
`vpnhotspot_v6_input` before the downstream `REJECT`. The kernel then delivers
it to whichever local socket owns that address and port, with the client's own
source address, as it does for IPv4 tethered clients. Replies from a local
socket that is not explicitly bound to a network reach the client through the
NAT66 prefix route in table 97, via netd's startup
`from all fwmark 0x0/0x10000 lookup local_network` rule. An accepted TCP socket
inherits netd's incoming local-network mark, because Android enables
`tcp_fwmark_accept`, so its replies use the downstream network's explicit rule
instead, and kernel replies reflect the same mark. That rule selects table 97
for a `LOCAL_NET_ID` downstream and the downstream's interface table for a
per-downstream local network; see
[NAT66 Routes, Address, And Policy Rule](#nat66-routes-address-and-policy-rule).
A socket explicitly bound to another network replies elsewhere, as it does to
IPv4 tethered clients. Like the other local/special returns, this traffic is
outside NAT66 admission and counting, so a blocked MAC reaches local apps on
these addresses as a blocked IPv4 client does.

Phone-local destinations otherwise follow Android's own local-delivery rules,
the same as for IPv4 tethered clients. The old proxy path bypassed them because
its connections came from uid 0 over loopback.

- netd's eBPF ingress filter drops packets arriving on a non-VPN interface for
  a VPN's own interface addresses, for every app. It also drops them for apps
  that a non-bypassable full-route VPN isolates to its interface, and for apps
  in lockdown while the VPN is down. System uids and loopback are exempt, and
  TCP RSTs and ingress FINs skip the owner match.
- A non-bypassable VPN's UID rule matches every unprotected socket in its UID
  ranges regardless of netId, at priority 13000 on Android 17 (12000 on
  Android 10), ahead of the implicit local-network rule at 20000 (17000 on
  Android 10). When the VPN's table routes the client's NAT66 address, for
  example through an IPv6 default route, an app it covers that does receive
  the client's datagram sends its UDP reply into the tunnel. An accepted TCP
  socket's inherited mark sets `protectedFromVpn`, so its replies skip that
  rule.

AOSP sources:

- implicit local-network rule:
  - https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/server/RouteController.cpp#654
  - https://android.googlesource.com/platform/system/netd/+/android-17.0.0_r1/server/RouteController.cpp#737
- VPN UID rule:
  https://android.googlesource.com/platform/system/netd/+/android-17.0.0_r1/server/RouteController.cpp#517
- incoming local-network mark:
  https://android.googlesource.com/platform/system/netd/+/android-17.0.0_r1/server/RouteController.cpp#481
- `tcp_fwmark_accept`:
  https://android.googlesource.com/platform/system/core/+/android-17.0.0_r1/rootdir/init.rc#316
- eBPF owner match and ingress filter:
  https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/bpf/progs/netd.c#741
  and
  https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/bpf/progs/netd.c#763

Without these returns, the daemon would proxy such traffic upstream. Its UDP
reply socket must then bind the exact original destination, which fails with
`EADDRINUSE` when a local app already holds that port.

Insertion at the chain heads is correct whenever the rules are installed: the
filter return lands above the session's downstream `REJECT`, and the mangle
return lands above the process-wide ACL gate and protocol jumps. Rules inserted
above them later are other `RETURN`s, rules scoped to another downstream, or the
gateway DNS preludes, whose destination is inside the excluded gateway prefix,
so their relative order does not matter.

Following addresses: routing subscribes to `RTNLGRP_IPV6_IFADDR` before its
initial dump, so a change that races the dump still produces a notification.
The session owner waits for notifications beside its commands. A notification,
including a netlink overrun, makes the owner re-dump through the session
request connection and reconcile only these rules. That refresh runs between
commands rather than racing them, so a command never cancels an in-flight
iptables mutation. Because every notification triggers a full dump, an overrun
needs no resubscription: the dump that follows it postdates every lost
notification. Router advertisement lifetime refreshes also send `RTM_NEWADDR`;
their dumps change nothing and run no iptables command. A refresh does not
re-resolve upstream interfaces or retry other failed mutations.

Rollback:

- an address that leaves the followed set: delete its pair, the
  `vpnhotspot_v6_tproxy` return first, then its `vpnhotspot_v6_input` return;
- replacement that removes the NAT66 session firewall, and session stop: delete
  each pair the same way with the rest of the applied list.

Clean:

- the NAT66 filter and mangle chain flush-and-delete removes every per-address
  return without any address knowledge.

Process death leaves these returns in place with the rest of the session's NAT66
firewall until Clean; the event connection closes with the process.

Failure and missing-state behavior:

- subscription failure: structured nonfatal `routing.local_ipv6.events`. The
  session follows no addresses and installs no per-address returns, so its
  phone-address traffic stays on the proxied path.
- initial dump failure: structured nonfatal `routing.local_ipv6.dump`. The
  session keeps the subscription with an empty set, so phone-address traffic
  stays proxied until the next notification dumps successfully.
- later dump failure: structured nonfatal `routing.local_ipv6.dump`. The
  previous set and its rules stay until a later notification dumps
  successfully.
- notification stream end: structured nonfatal `routing.local_ipv6.events`. The
  session withdraws every per-address return and stops following, because the
  set could only go stale and a stale return would keep a departed address off
  its upstream path.
- insert failure: the generic `routing.apply` nonfatal, carrying the failed
  restore input. The pair stays out of the applied list, so the next refresh or
  replacement retries it, and the address stays proxied until then. If the
  filter return's rollback delete also fails, `routing.iptables_delete` reports
  it, and the leftover filter return at most lets that address skip the
  downstream `REJECT` until Clean flushes the chain.
- delete failure: `routing.iptables_delete`. The pair stays applied, and the
  next reconcile that no longer wants it retries whichever half remains.
- expected races: rules match only the destination address and interface name,
  so an address that disappears before its rule is installed still gets an
  installable rule, which the deletion notification then removes. A departing
  interface does not fail the all-interface dump; its addresses leave the next
  dump. Deleting a rule that is already gone exits nonzero and counts as
  deleted, as for every session rule.

The address set is phone-owned kernel state, not something downstream clients
grow, so the daemon caps neither the set nor the rule count.

### NAT66 ICMPv6 Control Returns

Condition: `SessionConfig.ipv6_nat != null` and NAT66 startup committed at
least one TCP or UDP runtime capability.

External mutations:

- `ip6tables -t mangle -I vpnhotspot_v6_tproxy -i <downstream> -p icmpv6 --icmpv6-type 133 -j RETURN`
- `ip6tables -t mangle -I vpnhotspot_v6_tproxy -i <downstream> -p icmpv6 --icmpv6-type 135 -j RETURN`
- `ip6tables -t mangle -I vpnhotspot_v6_tproxy -i <downstream> -p icmpv6 --icmpv6-type 136 -j RETURN`

Rollback:

- delete the same rules.

These are Router Solicitation, Neighbor Solicitation, and Neighbor
Advertisement. They are local-link control traffic, not upstream NAT66 payload.

### NAT66 Filter Base Jumps

Condition: `SessionConfig.ipv6_nat != null` and NAT66 startup committed at
least one TCP or UDP runtime capability.

External mutations:

- `ip6tables -t filter -I INPUT -j vpnhotspot_v6_input`
- `ip6tables -t filter -I FORWARD -j vpnhotspot_v6_forward`
- `ip6tables -t filter -I OUTPUT -j vpnhotspot_v6_output`

Rollback:

- delete the same rules.

Clean deletes these repeatedly before flushing and deleting the target chains.

### NAT66 TPROXY Base Jump

Condition: `SessionConfig.ipv6_nat != null` and NAT66 startup returned at least
one runtime capability.

External mutation:

- `ip6tables -t mangle -I PREROUTING -j vpnhotspot_v6_tproxy`

Rollback:

- delete the same rule.

Clean deletes this repeatedly before flushing and deleting NAT66 mangle chains.

### Client IPv4 Allow And Stats Rules

Condition: for each committed unique `(client MAC, client IPv4 address)` pair
in `SessionConfig.clients`.

External mutations:

- `iptables -t filter -I vpnhotspot_acl -i <downstream> -m mac --mac-source <mac> -s <client-ipv4> -j vpnhotspot_stats`

Rollback:

- delete the same rule.

Condition: for each committed unique `(client MAC, client IPv4 address)` pair
in `SessionConfig.clients`.

External mutations:

- `iptables -t filter -I vpnhotspot_stats -i <downstream> -m mac --mac-source <mac> -s <client-ipv4> -j ACCEPT`
- `iptables -t filter -I vpnhotspot_stats -o <downstream> -d <client-ipv4> -j ACCEPT`

Rollback:

- delete the same rules.

The IPv4 forwarding admission whitelist is the committed `(MAC, IPv4)` pair.
Sent-direction packets must match both the client MAC and IPv4 source before
entering the stats chain, and the stats leaf that increments the counter is also
the rule that accepts the packet. Reply-direction packets are counted by
destination IPv4 after the generic `ESTABLISHED,RELATED` ACL path sends them
through `vpnhotspot_stats`; the client MAC is not available as the Ethernet
source on replies. A committed MAC with no IPv4 address leaves has no IPv4
forwarding capability and can still be authorized for DNS or NAT66.

During session replacement, if a client IPv4 address remains committed but its
owning MAC changes, routing first deletes that address's two
`vpnhotspot_stats` rules from the applied mutation set. Normal reconciliation
then reinserts the same rule shape for the new committed config, which resets
the kernel iptables counters before Kotlin associates the source with the new
MAC. Missing rules only clear the current process applied entry; command
execution failures are reported and the rest of reconciliation continues.

### Client NAT66 Allow Rules

Condition: `SessionConfig.ipv6_nat != null`, NAT66 startup committed at least
one TCP or UDP runtime capability, and for each committed unique client MAC in
`SessionConfig.clients`.

External mutation:

- `ip6tables -t mangle -I vpnhotspot_acl -m mac --mac-source <mac> -j RETURN`

Rollback:

- delete the same rule.

The NAT66 base ACL drop rule is process-wide; client allow rules return before
that drop. NAT66 TCP/UDP rules still match MAC again to select that MAC's
listener port. NAT66 ICMPv6 uses the ACL result for admission, then the
dispatcher verifies the queued packet's `NFQA_HWADDR` against the same committed
MAC set before proxying or counting it.

## Process-Wide NAT66 Firewall Base

[`control.rs`](../../mobile/src/main/rust/vpnhotspotd/src/control.rs) calls
`ensure_ipv6_nat_firewall_base` before starting the first session that requests
NAT66. This is outside per-session desired state. Failure is reported as a
structured nonfatal tied to the start-session call and disables NAT66 for that
session start; the IPv4 session may still continue.

External mutations:

- ensure `ip6tables -t mangle` chain `vpnhotspot_acl`
- ensure `ip6tables -t mangle` chain `vpnhotspot_v6_acl_gate`
- ensure `ip6tables -t mangle` chain `vpnhotspot_v6_protocols`
- ensure `ip6tables -t mangle` chain `vpnhotspot_v6_tproxy`
- `ip6tables -t mangle -I vpnhotspot_acl -j DROP`
- `ip6tables -t mangle -I vpnhotspot_v6_tproxy -j vpnhotspot_v6_protocols`
- `ip6tables -t mangle -I vpnhotspot_v6_tproxy -j vpnhotspot_v6_acl_gate`

Because these rules are inserted with `-I`, session rules must preserve the
effective `vpnhotspot_v6_tproxy` order documented above: local-link ICMPv6
control returns, gateway DNS ACL/protocol handling, local/special destination
and phone address returns, generic ACL gate, then protocol interception. That ordering is part of
the NAT66 blocking contract: blocked MACs hit the base ACL drop before
daemon-owned DNS, upstream TCP/UDP TPROXY, or ICMPv6 NFQUEUE, while local or
special destinations outside those daemon-owned paths do not use the ACL as an
admission gate.

Process/session cleanup:

- `delete_ipv6_nat_firewall_base` deletes those three base rules in reverse
  order.
- Clean also flushes and deletes the NAT66 mangle chains after removing base
  jumps.

## Static Address Replacement

`ReplaceStaticAddressesCommand` is not session routing, but it is implemented
under `routing/` and mutates interface addresses.

External mutations:

- for each requested address: replace address `<address>/<prefix-len>` on
  `dev <interface>` using rtnetlink.
- after dumping current addresses for the interface: delete every non-loopback
  address whose `(address, prefix_len)` is not in the requested set.

Expected benign errors:

- `EEXIST` while replacing a requested address is accepted.
- missing address while deleting stale addresses is accepted.

This command reconciles one interface to the requested address set. It is not a
session rollback mechanism.

## Clean Mutations

`CleanRoutingCommand` is deterministic cleanup. It does not use session memory.

### Policy Rules And Routes

External mutations:

- repeatedly delete IPv6 policy rules at the NAT66 daemon priority, including
  protocol rules, fwmark fallback rules, and any interrupted detached probe
  rule.
- repeatedly delete IPv4 policy rules at `<primary-priority>`.
- repeatedly delete IPv4 policy rules at `<fallback-priority>`.
- repeatedly delete IPv4 policy rules at `<upstream-disable-priority>`.
- flush IPv6 routes from table 900.

For every current interface name:

- reconstruct the deterministic NAT66 prefix from the Clean prefix seed and the
  interface name.
- delete address `<nat66-gateway>/<prefix-len>` from that interface.
- delete table 97 unicast route `<nat66-prefix> dev <interface>`.
- delete unicast route `<nat66-prefix> dev <interface> table <1000 + ifindex>`
  using the interface's current ifindex. A route left in an older ifindex's
  table went with its device, because the kernel deletes routes through a
  device that is removed.

Missing address or route is expected. For address deletion, this includes an
interface that still exists but has no IPv6 address state. Other errors are
reported as structured nonfatal cleanup reports.

### IPv4 Firewall Cleanup

External mutations:

- repeatedly delete `iptables -t mangle PREROUTING -j vpnhotspot_dns_tproxy`.
- repeatedly delete `iptables -t filter INPUT -j vpnhotspot_dns_input`.
- repeatedly delete `iptables -t filter FORWARD -j vpnhotspot_acl`.
- repeatedly delete `iptables -t nat PREROUTING -j vpnhotspot_dns_nat`.
- repeatedly delete `iptables -t nat POSTROUTING -j vpnhotspot_masquerade`.
- restore IPv4 mangle table input that flushes or creates
  `vpnhotspot_dns_tproxy`, then deletes it.
- restore IPv4 filter table input that flushes or creates
  `vpnhotspot_dns_input`, `vpnhotspot_acl`, and `vpnhotspot_stats`, then
  deletes them.
- restore IPv4 nat table input that flushes or creates `vpnhotspot_dns_nat`
  and `vpnhotspot_masquerade`, then deletes them.

The IPv4 mangle `vpnhotspot_dns_tproxy` cleanup is legacy-shaped cleanup kept
in Clean. IPv4 nat cleanup must stay scoped to app-owned jumps and chains.

### IPv6 Firewall Cleanup

External mutations:

- repeatedly delete IPv6 block jumps:
  - `ip6tables -t filter INPUT -j vpnhotspot_filter`
  - `ip6tables -t filter FORWARD -j vpnhotspot_filter`
  - `ip6tables -t filter OUTPUT -j vpnhotspot_filter`
- repeatedly delete NAT66 filter jumps:
  - `ip6tables -t filter INPUT -j vpnhotspot_v6_input`
  - `ip6tables -t filter FORWARD -j vpnhotspot_v6_forward`
  - `ip6tables -t filter OUTPUT -j vpnhotspot_v6_output`
- repeatedly delete NAT66 mangle jump:
  - `ip6tables -t mangle PREROUTING -j vpnhotspot_v6_tproxy`
- restore IPv6 filter table input that flushes or creates, then deletes:
  - `vpnhotspot_filter`
  - `vpnhotspot_v6_input`
  - `vpnhotspot_v6_forward`
  - `vpnhotspot_v6_output`
- restore IPv6 mangle table input that flushes or creates, then deletes:
  - `vpnhotspot_acl`
  - `vpnhotspot_v6_acl_gate`
  - `vpnhotspot_v6_protocols`
  - `vpnhotspot_v6_tproxy`

Flushing `vpnhotspot_v6_input` and `vpnhotspot_v6_tproxy` also removes every
NAT66 phone address return, so Clean needs no address state.

Missing rules or chains are expected after partial startup or prior cleanup.
Unexpected restore failures are reported.

## Priority And Table Model

VPNHotspot policy rules live in the gap between AOSP local-network and
tethering rules. The code names four base priorities:

| Role | Android 12+ | Android 10/11 |
| --- | ---: | ---: |
| NAT66 daemon table lookup | `20600` | `17600` |
| Primary upstream table lookup | `20700` | `17700` |
| Fallback upstream table lookup | `20800` | `17800` |
| Downstream unreachable guard | `20900` | `17900` |

Android 10 and 11 use the same bases minus `3000` because AOSP local-network
and tethering priorities were lower before Android 12.

The daemon uses these table conventions:

- table 900 is VPNHotspot's daemon table for NAT66 local interception;
- table 97 is Android's shared `local_network` route table;
- upstream and downstream interface tables use Android's `ifindex + 1000`
  convention.

Table 900 may be flushed by Clean because it is reserved by VPNHotspot. Table
97 and interface tables must not be flushed; delete only reconstructed
VPNHotspot NAT66 routes from them.

## Guardrails

- Do not add a route, rule, address, mark, chain, or firewall rule without
  adding it to this mutation catalog.
- Do not make cleanup depend on app preferences, databases, caches, or daemon
  memory for state that can outlive the process.
- Do not install interception for a DNS or NAT66 runtime capability that failed
  to start.
- Do not publish a per-MAC DNS or NAT66 capability unless both the daemon
  resource and matching routing rule committed. Cancel staged resources whose
  rules failed.
- Do not disable netd NAT unless the daemon has an app-owned ownership token for
  the exact state being disabled.
- Do not flush shared platform tables or shared firewall base chains.
