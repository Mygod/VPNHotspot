# Traffic Accounting And Blocking

Traffic control is MAC-facing. IP addresses are implementation metadata, not
the product identity.

This document describes the daemon-side accounting and admission model. The
database schema still stores `TrafficRecord.ip` and `TrafficRecord.upstream` for
compatibility, but the live identity used for blocking, accounting, and UI
aggregation is:

- client MAC address;
- downstream interface;
- counter source;
- counter epoch.

## Scope

Blocking and counters apply only at VPNHotspot-owned upstream boundaries:

- IPv4 forwarding and NAT rules installed by VPNHotspot;
- DNS queries intercepted by VPNHotspot and sent through Android's resolver API;
- NAT66 traffic proxied by the Rust daemon.

Traffic that stays local to the downstream network is unrestricted and
uncounted by this feature. Native or system-owned IPv6 forwarding outside the
daemon NAT66 proxy is also uncounted.

Client authorization is whitelist based. A downstream MAC that is not in the
committed client set must not enter an upstream forwarding, DNS proxy, or NAT66
proxy path. The daemon must not recover client identity by looking up source IP
addresses in neighbour state.

## Client Identity

`ClientConfig` entries are keyed by MAC and may have an empty IPv4 list. A
client can be admitted for DNS and NAT66 from a valid ARP or NDP neighbour that
has a six-byte link-layer address; an IPv4 address is not required for IPv6
NAT66 proxying or accounting.

IPv4 forwarding still uses real client IPv4 addresses as hidden kernel counter
leaves. Those addresses keep reply-direction counters accurate without using
Android connmark bits, but they do not become the UI identity.

Daemon-owned MAC-only sources persist with `TrafficRecord.ip = 0.0.0.0` and a
reserved source marker in `TrafficRecord.upstream`:

| Source | Stored `upstream` marker |
| --- | --- |
| DNS resolver proxy | `/dns` |
| NAT66 TCP proxy | `/nat66/tcp` |
| NAT66 UDP proxy | `/nat66/udp` |
| NAT66 ICMPv6 Echo/error proxy | `/nat66/icmpv6` |

These markers are opaque compatibility payloads. They are not upstream
interface names and must not be shown as user-visible upstreams.

## Admission Handoff

DNS and NAT66 TCP/UDP use per-MAC listener ownership:

- each allowed MAC gets its own daemon resources for each started protocol
  capability;
- routing installs MAC-matched DNAT or TPROXY rules to that MAC's listener
  ports;
- accepted DNS queries, TCP connections, UDP associations, and NAT66 gateway
  DNS special cases inherit the MAC from the listener they entered through.

The listener port is not a client-controlled identity token. IPv4 DNS listener
ports accept only packets whose conntrack original destination was the
downstream gateway on port 53. NAT66 listener ports are internal `::1` TPROXY
endpoints reached only through the MAC-matched TPROXY path; direct local or
special-destination traffic to those ports does not reach the listener and
falls through the input reject path.

NAT66 ICMPv6 Echo uses one process-wide NFQUEUE path. The queue number is
`30000`. Routing queues eligible Echo Requests only after the NAT66 ACL has
admitted the source MAC. The ICMP dispatcher then requires `NFQA_HWADDR` to
carry exactly six source-MAC bytes, and that MAC must still belong to the
committed session client set.

If queued ICMP lacks usable hardware-address metadata, carries a non-six-byte
address, or names a MAC that is no longer allowed, the daemon drops the packet,
reports a structured nonfatal, and does not count it. There is no source-IPv6
fallback.

## Counter Sources

The daemon reports cumulative structured counters. Kotlin maps each counter
source to the existing persistence shape:

- IPv4 forwarding: store the real IPv4 address and no source marker in
  `upstream`;
- daemon-owned DNS or NAT66 source: store `ip = 0.0.0.0` and the reserved source
  marker above.

The structured counter source is the stable active-recorder key. The persisted
`upstream` marker is only the current no-migration storage representation.
Routing keeps the recorder's active client set in sync with allowed MACs, while
IPv4 neighbour entries only add or remove IPv4 forwarding rows. A client losing
its IPv4 neighbour therefore does not stop DNS or NAT66 polling while the MAC is
still allowed.
When duplicate IPv4 forwarding counter rules exist after interrupted cleanup,
the daemon keeps the first matching iptables rule per direction because that is
the rule whose counters the kernel updates.

Packet counters are meaningful only for sources with discrete messages:

- DNS UDP: one sent packet per resolver query and one received packet per
  resolver response;
- DNS TCP: one sent or received unit per framed DNS message;
- NAT66 UDP: one sent or received packet per UDP datagram;
- NAT66 ICMPv6: one sent or received packet per daemon-owned Echo or upstream
  ICMPv6 error message;
- NAT66 TCP: sent packet count is a connection count for successfully opened
  upstream TCP connections; byte counters carry the actual stream volume.

Byte counters are authoritative for all daemon-owned sources. DNS counts the
payload bytes handed to and returned from Android's resolver API. NAT66 counts
daemon-owned upstream socket I/O at the proxy boundary; it does not estimate IP
headers, TCP handshakes, segmentation, retransmits, DNS-over-TLS transport, or
physical egress behavior below Android's resolver/network APIs.

Client-visible DNS errors generated by the daemon are not resolver API return
traffic. They are not counted unless a query was actually handed to Android's
resolver; in that case the sent query side may be counted without a received
response side.

## Counter Epochs

Every cumulative counter carries an opaque epoch. The epoch changes whenever
the backing cumulative counter can reset without Kotlin first flushing and
removing the active record.

Kotlin chains records only within the same source epoch. If the same
MAC/downstream/source appears with a new epoch, the recorder leaves the old
chain terminal and starts a new one. This preserves historical totals while
limiting loss to increments since the last successful read.

Daemon-owned DNS and NAT66 counters live in session memory and use a
session-instance epoch. A daemon or session restart therefore starts new
recorder chains instead of attaching reset in-memory counters to old history.
Removed per-MAC runtime sources move their final counters into a retired
snapshot. A traffic counter read returns active plus retired counters and then
drops the retired entries. Retired delivery is at-most-once; normal stop,
replacement, and routing teardown paths must still read counters before
destroying daemon-owned state.
The read snapshots daemon-owned counters while holding the session slot so a
concurrent stop or Clean cannot destroy counters after the read has observed the
session as active.

## Failure Semantics

Per-MAC DNS and NAT66 TCP/UDP setup is capability scoped. If one MAC/protocol
listener or its matching routing rule fails, the daemon reports a structured
nonfatal with downstream, MAC, and capability, cancels any staged resource for
that failed capability, and keeps the rest of the session running.

A per-MAC capability is committed only after the daemon resource and matching
routing rule both exist. The committed session must not keep a hidden listener
with no reachable rule, and routing must not keep a rule whose daemon resource
was rolled back.

IPv4 client removal removes allow rules and hidden IPv4 counter leaves. If an
IPv4 address remains present but moves to a different MAC, routing deletes the
address's hidden counter leaves before reconciliation so the reinserted leaves
start a new kernel counter epoch for the new owner. The daemon does not manage
conntrack state. DNS and NAT66 are daemon-owned, so removing a MAC cancels that
MAC's DNS children, TCP tasks, UDP associations, ICMP Echo allocations, and
ICMP/UDP error registrations.

If IPv4 forwarding counter readout fails, the daemon reports a structured
nonfatal tied to the traffic-counter call and still replies with any
daemon-owned DNS/NAT66 counters it already snapshotted. IPv4 forwarding counters
live in kernel rules, so later reads can recover them while those rules still
exist; daemon-owned retired counters are more volatile and must not be discarded
because the IPv4 read failed.
