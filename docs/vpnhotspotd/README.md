# vpnhotspotd Internals

`vpnhotspotd` is the daemon used by VPNHotspot intended for all long-running background root-side work, including but not limited to routing state, DNS proxying, neighbour monitoring, and IPv6 NAT mode.
The same binary also has an app-UID entry point for Shizuku mode, which owns a TUN and no system state; the two share the launcher and the frame format and nothing else, because none of the root-side mutations are permitted at the app UID.
The split is designed this way since root-side JVM daemon is much more expensive and should be avoided as much as possible.
These docs describe the daemon's internal ownership model and cleanup invariants.
IPC documentations not included and should refer to [`mobile/src/main/proto/daemon.proto`](../../mobile/src/main/proto/daemon.proto).

## Source Map

- [`bootstrap.rs`](../../mobile/src/main/rust/vpnhotspotd/src/bootstrap.rs) owns
  the Shizuku-mode app-UID handshake and the received TUN descriptor.
- [`control.rs`](../../mobile/src/main/rust/vpnhotspotd/src/control.rs) owns the
  daemon control loop, active calls, event-style (kotlin Flow) calls that
  remain active for sessions or monitors, session slots, and the neighbour
  monitor slot.
- [`session.rs`](../../mobile/src/main/rust/vpnhotspotd/src/session.rs)
  composes one session for one downstream interface from DNS, optional NAT66,
  and routing runtimes.
- [`routing.rs`](../../mobile/src/main/rust/vpnhotspotd/src/routing.rs) and
  [`routing/`](../../mobile/src/main/rust/vpnhotspotd/src/routing/) own
  reversible route, rule, address, firewall, forwarding, and static-address
  mutations.
- [`nat66/`](../../mobile/src/main/rust/vpnhotspotd/src/nat66/) owns the IPv6
  NAT proxy runtimes and helper protocol state.
- [`dns.rs`](../../mobile/src/main/rust/vpnhotspotd/src/dns.rs) owns the daemon
  DNS listeners and Android resolver handoff.
- [`traffic.rs`](../../mobile/src/main/rust/vpnhotspotd/src/traffic.rs) owns
  daemon traffic-counter reads and the daemon-to-Kotlin counter reporting
  boundary.
- [`netlink.rs`](../../mobile/src/main/rust/vpnhotspotd/src/netlink.rs) owns the
  owner-scoped rtnetlink request and multicast-only event connections.
- [`neighbour.rs`](../../mobile/src/main/rust/vpnhotspotd/src/neighbour.rs)
  owns neighbour-monitor connections and converts netlink neighbour and bridge
  topology state into daemon events.
- [`ipsec.rs`](../../mobile/src/main/rust/vpnhotspotd/src/ipsec.rs) owns the
  optional Android 12+ IPsec forwarding-policy probe and emits session events
  for the Kotlin routing owner to perform the hidden Netd write only when
  needed.
- [`app_session.rs`](../../mobile/src/main/rust/vpnhotspotd/src/app_session.rs) owns
  the Shizuku session's control loop: it applies each level-triggered config, retires
  what the two axes invalidate before acknowledging, and owns the ingress task. It also
  owns the control writer, so a report can leave whenever it happens rather than only in
  reply to a config.
- [`tun_reader.rs`](../../mobile/src/main/rust/vpnhotspotd/src/tun_reader.rs) owns the
  TUN descriptor, the per-packet admission gate, and every piece of client-keyed dataplane
  state, since it is the only reader of that descriptor. It also owns every task those
  transports started: it selects on their completions beside their traffic, joins each one,
  and refunds only then. It answers each config once retirement is complete, which is what
  lets the session acknowledge an epoch truthfully, and it runs the same quiesce-and-join
  over every owner before it returns, however its loop ended.

  It owns both ends of the fixed reservation, and that is a single ordering rather than two
  conventions. `prepare` builds the aggregate, reserves every fixed byte, and only then
  constructs the writer's three channels, so nothing the session allocates exists before the
  ledger that pays for it; there is no other production path to a writer or a queue. The
  teardown runs the reverse and every exit funnels through it, including a failure to start
  the traffic owners: the owners stop, join and refund, then the output owner is dropped -
  taking the Identification table and the last writer senders with it - and only once the
  settlement channel closes, which is the egress task's last act, are the fixed bytes given
  back. Releasing earlier would be bytes returned for allocations that still exist.
- [`dispatch.rs`](../../mobile/src/main/rust/vpnhotspotd/src/dispatch.rs) is one read
  dispatched to the transport that owns it, and the counters for what it could not place.
  Apart from the ingress task because it is the part with no lifecycle in it, and because the
  same path has to run again on the datagram a fragment completed.
- [`tun_writer.rs`](../../mobile/src/main/rust/vpnhotspotd/src/tun_writer.rs) owns the
  single TUN egress path: the bounded queue, the retirement gate, final size validation,
  and the writability wait. That gate is both halves of the handover purge at once - a
  packet carries the generation and epoch it was produced under, so what a sweep left
  queued is dropped and the terminal packets the sweep writes are not. It decides nothing
  about what a packet contains: the size policy, the Identification and source fragmentation
  all happened in `output.rs` before the packet was queued.

  It is, though, the only owner that knows whether a packet reached the wire and when, so
  while the session continues every guarded IPv4 packet it accepts ends in exactly one
  settlement back to the ingress task - written with the instant the write returned, or
  unwritten for a stale dequeue, a validation refusal or a retirement that preempted a
  blocked write. The endings of the session itself are the exception: a fatal write, a lost
  settlement path or a cancellation stops the loop with registrations outstanding, and the
  successor session's opening quarantine is what covers them. A write that returns neither
  the whole packet nor an error is one of those fatal cases rather than a success, because a
  packet-atomic descriptor taking part of a packet has broken the premise the design rests
  on and the bytes on the wire are a truncated packet.
- [`shared/classify.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/classify.rs)
  classifies TUN packets into the three shared principals. Platform-neutral and unit
  tested.
- [`egress.rs`](../../mobile/src/main/rust/vpnhotspotd/src/egress.rs) owns the
  Shizuku dataplane's selected-network sockets: the bind to the selected `Network`,
  per-message hop limits, IPv4 DF modes, the error queue, and the received hop and
  interface metadata. Raw `libc` lives there because `socket2` exposes none of those.
- [`shared/packet_writer.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/packet_writer.rs)
  owns TUN-side packetization: final size validation, source fragmentation for both
  families, and the size policy that decides which datagrams may clear DF. Platform-neutral
  and unit tested; the task that owns the descriptor and its bounded queue lives beside the TUN.
- [`shared/ipv4_identification.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/ipv4_identification.rs)
  is the guarded IPv4 Identification allocator and the temporal state that makes it exact: a
  per-tuple sequence that ends rather than wraps, a count of packets the writer has not settled,
  the latest moment one of them reached the wire, and a session-opening window that covers what a
  predecessor may have written. Beside the packetization rather than inside it, because when a
  value may be used again is a question about wire time rather than about bytes. Platform-neutral
  and unit tested with injected instants.
- [`udp.rs`](../../mobile/src/main/rust/vpnhotspotd/src/udp.rs) owns the Shizuku
  dataplane's UDP relay: one endpoint-independent, address-filtered mapping per
  TUN-visible source, its permitted-remote records, its timers, and the per-mapping
  receive tasks whose replies it packetizes and writes.
- [`reply.rs`](../../mobile/src/main/rust/vpnhotspotd/src/reply.rs) is one egress socket's
  reply reader and the events it reports, serving both the UDP relay and Echo. Apart from the
  tables because it shares no state with them: it owns a socket reference and a channel, and
  every check, allocator and write stays with the table. Waits on error readiness as well as
  readability, because a queued ICMP error raises only `EPOLLERR` and waiting on readability
  alone never wakes for one.
- [`echo.rs`](../../mobile/src/main/rust/vpnhotspotd/src/echo.rs) owns relayed Echo: one
  session per outstanding ping, keyed by the remote and the sequence the daemon substituted,
  because an unprivileged ping socket overwrites the identifier and passes only the sequence
  through.
- [`echo_session.rs`](../../mobile/src/main/rust/vpnhotspotd/src/echo_session.rs) owns the
  outstanding pings and the substituted sequence each is known by. Two lookups that are not
  interchangeable: a reply is found by `(remote, sequence)`, which makes the address filter
  structural, and an error about a request only by the sequence, because a ping socket's errors
  name no remote - so that one answers "exactly one" or "more than one" rather than picking.
- [`echo_socket.rs`](../../mobile/src/main/rust/vpnhotspotd/src/echo_socket.rs) owns the ping
  sockets those sessions send through, one per family and generation. Apart from the table
  because a descriptor is retired by a different protocol: cancel, join the receive task, drop
  this side's share of the socket, and refund only then.
- [`shared/echo_wire.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/echo_wire.rs) is
  the strict parse of a client Echo Request, the request written to a ping socket, the reply
  built back toward the client, and the client's own request rebuilt as the quote inside an
  error about it. Platform-neutral and unit tested.
- [`gateway.rs`](../../mobile/src/main/rust/vpnhotspotd/src/gateway.rs) holds the interface's
  own addresses and the one decision they are for: which address an originated ICMP error
  speaks from, and when there is no honest answer. Owned by the ingress task rather than by a
  transport, because a reassembly timeout is owed for whatever was being reassembled.
- [`shared/icmp_translate.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/icmp_translate.rs)
  decides whether an ICMP error a *remote* sent may be repeated to a client, and builds it from
  the router's own address rather than the interface's. Refuses what needs the send history to
  correlate, and values no path could carry. Platform-neutral and unit tested.
- [`send_failure.rs`](../../mobile/src/main/rust/vpnhotspotd/src/send_failure.rs) reads one
  refused egress send into the meaning a relay acts on. Shared by UDP and Echo because both see
  the same errnos and the errno-to-meaning mapping is what must not drift; what each relay
  *does* about a meaning stays with the relay.
- [`shared/extension.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/extension.rs)
  walks one IPv6 extension chain, bounded, and removes it so the transport parses run on the
  result unchanged. Refuses source routing and a misplaced hop-by-hop header; leaves a Fragment
  header in place for reassembly. Platform-neutral and unit tested.
- [`shared/send_history.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/send_history.rs)
  remembers what one mapping recently sent - destination, length, digest, hop limit, never the
  payload - so an error claiming to be about one datagram can be shown to describe a real one.
  Correlation ends at the first resolution of any kind. Platform-neutral and unit tested.
- [`shared/reassembly.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/reassembly.rs)
  holds the bounded ingress reassembly contexts for both families and hands back datagrams with
  the fragmentation removed, so the strict transport parses run on them unchanged. Overlaps
  discard the datagram, the byte ceiling charges buffer growth rather than fragment size, and an
  expiry yields fragment zero so the caller can answer it. Platform-neutral and unit tested.
- [`tcp.rs`](../../mobile/src/main/rust/vpnhotspotd/src/tcp.rs) owns terminated TCP: the flow
  table, SYN interception by listening on the client's chosen destination, the `smoltcp` interface
  and socket set, and the poll that advances them. It is also where a config change decides what to
  retire, by axis rather than wholesale: the epoch retires every flow, while the generation retires
  exactly the flows holding a selected-network socket - which a virtual-DNS transport does not, so
  it is left running with its client. What each flow *is* is recorded when it is opened, because an
  idle DNS transport has no query outstanding and a kind inferred from one would reset it.
- [`tcp/terminal.rs`](../../mobile/src/main/rust/vpnhotspotd/src/tcp/terminal.rs) owns how a terminating
  flow ends, and is the only code that gives one back. Four endings converge on one `reclaim`: a clean
  worker terminal from a flow nobody cancelled *detaches* it, because both workers return while the client
  is still closing and removing the flow there would take its half of the teardown away; a detached flow's
  client finishing is found by this owner scanning its own rows, with no task of its own and no *per-flow*
  timer task behind it - its teardown is still scheduled, by the combined smoltcp-and-outer deadline; a
  failed or reported terminal resets the client and ends the flow at once; and a retirement settles a
  detached row directly, because no terminal will ever name it again. Its tests live with
  `tcp/lifetime.rs`'s harness, which is what drives real client phase transitions.
- [`tcp/lifetime.rs`](../../mobile/src/main/rust/vpnhotspotd/src/tcp/lifetime.rs) owns how long a
  terminated flow may sit idle in this daemon's *outer* state - Android's inner IPv4 NAT keeps conntrack of
  its own and none of it is mirrored, configured or timed from here. Three things live together because
  they are one rule: RFC 5382 section 5's classification of the actual post-poll `smoltcp` state into the
  two-hour-four-minute established floor and the four-minute transitory one, what counts as the observable
  activity that rearms a phase's whole floor, and the retirement a passed floor begins. It also owns the
  single deadline the ingress task sleeps on, which is the earlier of smoltcp's own protocol timers and the
  earliest floor any live flow holds - a cancelled flow is excluded, because one waiting to be joined would
  otherwise keep a passed deadline as the earliest in the table. Nothing is removed or refunded there: an
  expiry cancels only that flow's own token, so its upstream closes the ordinary way rather than with the
  `SO_LINGER(0)` a generation sweep selects, and the record and its charge go when the flow is finally
  settled - which is not the same moment its worker finished, because a clean terminal from a flow nobody
  asked to stop *detaches* it and leaves the client's teardown to run: see
  [Shizuku Mode](shizuku.md#a-flow-can-outlive-its-worker).
  Both this walk and the config retirement's step through the round-robin order the fair queue already
  registers every admitted flow into, rather than collecting what is due into a list: that order holds each
  live handle exactly once, so it is the list, and building one would be scratch no lease covers on a path a
  stopping session still runs. A row an expiry has already cancelled is skipped by a following config
  retirement and waited for by it, so the socket is aborted once and the client is told once - a reset only
  where there is a remote endpoint to send one to, and silence where there is not.
- [`tcp_device.rs`](../../mobile/src/main/rust/vpnhotspotd/src/tcp_device.rs) is the adapter between
  the TUN and that stack: pushed ingress, and egress collected for the common writer. It advertises
  the downstream floor as the MTU, which is what makes every segment fit the narrowest downstream.
- [`tcp_dns.rs`](../../mobile/src/main/rust/vpnhotspotd/src/tcp_dns.rs) serves one DNS-over-TCP flow
  from the platform resolver instead of an upstream connection, framing RFC 1035's two-byte length
  prefix. Interchangeable with the splice from the engine's side, including how it ends: its task
  completing releases the descriptor, and whether that also *retires* the flow depends on the client - a
  clean completion whose client is still closing detaches instead, and the flow keeps its socket and its
  charge through `LAST-ACK` or `TIME-WAIT`. What it asks its owner for travels on the flow's own depth-one
  control pair, built and charged with the flow, so no per-query channel appears before that query's
  grant does.
- [`tcp_dns/transactions.rs`](../../mobile/src/main/rust/vpnhotspotd/src/tcp_dns/transactions.rs) owns
  those transactions, apart from the transports that asked for them, because a retirement must sweep a
  client's transport at once and must not cancel a submitted query at all - so a transaction holds the
  reserved resolver slot until the platform is genuinely done with it. It is a prepared, charged table
  the ingress owner polls rather than a task per query: a row is inserted irrevocably and the platform
  is then called synchronously, so there is no state in which Android holds a question this table is not
  accounting for. Each query is admitted at the length its prefix announces *before* the message is
  stored, and is stamped with the config and the `Network` current when the ingress owner accepts it -
  so one connection's questions may belong to different selections, and an answer from a selection the
  session has left becomes that query's own SERVFAIL on a stream that stays usable. A query with no
  network to resolve on, or none the descriptor floor has room for, is answered the same way; only one
  whose bytes do not fit at all is skipped.
- [`tcp/dns.rs`](../../mobile/src/main/rust/vpnhotspotd/src/tcp/dns.rs) is the engine's side of that
  conversation: admitting an announced length, accepting the exact query at the commit boundary that
  fixes its stamp and `Network`, answering one the platform will not be asked about, classifying a
  settled answer against the config current now, and ending a delivery when the client's stack has
  really taken it.
- [`tcp_flow.rs`](../../mobile/src/main/rust/vpnhotspotd/src/tcp_flow.rs) owns one flow's upstream
  socket and the splice between it and the stack, where the bounded bidirectional backpressure lives.
  Every wait in it races the flow's token, so a retirement is never held up by a peer.
- [`workers.rs`](../../mobile/src/main/rust/vpnhotspotd/src/workers.rs) is the fence
  between a worker's completion and its owner's accounting: records are reachable only through it, and
  one comes back only once its task has run to completion, so a refund cannot precede a close.
- [`shared/preempt.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/preempt.rs) is the set of
  waits a worker may be preempted out of - a write, a half-close, an event handover - each racing the
  worker's own cancellation.
- [`shared/reporter.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/reporter.rs) owns nonfatal
  coalescing on the producer's side of the queue, plus the one task that closes a coalescing window,
  and the one *finalizer* that cancels and joins it. Ending a conversation's reporting is a task
  rather than a `Drop`, because closing admission, draining producers, joining the window task and
  handing the last summaries to a serial writer all have to wait. It also owns the reporter's
  *registration*: weak, so being reachable from every packet path is not being kept alive, and
  single, so two conversations cannot each believe they own reporting - held busy until that
  finalizer has actually completed.
- [`shared/tasks.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/tasks.rs) is the task
  ownership both conversations share: the handles a session must watch - whose completion it selects
  on, and which it joins exactly once each through one cleanup path - and the background set a
  process cancels and joins before it stops. Platform-neutral and unit tested.
- [`shared/failure.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/failure.rs) is which side
  of an operation failed: the daemon's own local setup, which is a structured report naming the step,
  or what the peer, the path or the platform answered, which is an ordinary per-record outcome a
  client chooses the volume of. Platform-neutral and unit tested.
- [`shared/icmp_error.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/icmp_error.rs) builds the
  ICMP errors the daemon originates about its own forwarding decisions - expired hop limits and
  oversized DF-set sends - from the interface's address, with a truncated quote. Platform-neutral and
  unit tested.
- [`shared/tcp_wire.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/tcp_wire.rs) peeks just
  enough of a segment to route it: the four-tuple, the hop limit, and whether it is a SYN.
- [`shared/udp_wire.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/udp_wire.rs)
  is the strict parse of a relayed client datagram and the construction of its reply,
  checksums included. Platform-neutral and unit tested.
- [`budget.rs`](../../mobile/src/main/rust/vpnhotspotd/src/budget.rs) is the single
  admission owner for traffic-driven Shizuku dataplane state, with a ceiling measured
  from descriptors at session start rather than chosen, plus the nested ceiling on
  concurrent resolver transactions.
- [`output.rs`](../../mobile/src/main/rust/vpnhotspotd/src/output.rs) is the one place a
  TUN-side datagram becomes packets: the DF decision against the downstream floor, source
  fragmentation against the interface, and the shared Identification allocators. Shared
  because reassembly tuples do not include ports, so per-producer allocators would
  mis-splice datagrams between producers. One per session and kept across every generation and
  epoch, because a handover replaces sockets and not the fragments a receiver is still holding.
  It is also where the writer's settlements are applied, which is the only path by which a wire
  time reaches the allocator.
- [`virtual_dns.rs`](../../mobile/src/main/rust/vpnhotspotd/src/virtual_dns.rs) owns the
  Shizuku dataplane's virtual-DNS endpoints over UDP: the nested query ceiling, the handoff to the
  platform resolver, discarding answers by either axis, and SERVFAIL on refusal. Its DNS-over-TCP
  counterpart is `tcp_dns.rs`, which reaches the same outcomes through the terminating engine.
- [`shared/dns_wire.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/dns_wire.rs) holds the
  attacker-facing half of both: RFC 1035's length-prefixed stream framing, which announces a length
  before anything is stored and fills only a buffer whose capacity was admitted for it, and the rule
  that turns a platform outcome into that message's own SERVFAIL. Platform-neutral and unit tested.
- [`shared/dns_debt.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/dns_debt.rs) owns which
  grant covers which bytes across a DNS-over-TCP connection and the queries it submits: an idle
  connection's token, a framed query's record and bytes, the second tier that answers a query no
  descriptor is left for, the token transfer when a transport closes over a question, the delivery
  that outlives the settled transaction, and the bounded quarantine a token goes into when the
  platform accepted a query this process can no longer observe. Platform-neutral and unit tested.
- [`resolver.rs`](../../mobile/src/main/rust/vpnhotspotd/src/resolver.rs) owns one
  resolver transaction on a chosen `Network` through `android_res_nsend`, including the
  descriptor protocol its synchronous result reader requires and the poll an owner can drive it
  with instead of a task. It also names the three outcomes of a submission rather than two:
  never reached, accepted and observable, and reached but unobservable - the last being a
  per-UID slot Android holds that this process can no longer watch. The same distinction
  applies to a *completion*, because the watching can be lost after it started, so a poll
  answers with an outcome rather than a plain result. Both stay typed all the way to the owner
  that acts on them: collapsing them into one error is what once quarantined the logical token
  of a query the platform had refused. Reached only from the app-UID TestNetwork path;
  `virtual_dns.rs` is its one caller, and root's DNS proxy keeps its own resolver in `dns.rs`.
- [`report.rs`](../../mobile/src/main/rust/vpnhotspotd/src/report.rs) and
  [`shared/protocol.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/protocol.rs)
  build structured daemon reports. The *builders* are shared by both modes; the delivery is
  not. Root keeps master's shape - one coalescing task behind a process-global channel,
  keyed with [`shared/nonfatal.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/nonfatal.rs)
  and flushed once at the end. An app-UID session owns its own reporter from
  [`shared/reporter.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/reporter.rs), held
  weakly by a registry and site-keyed by
  [`shared/reporter/coalescer.rs`](../../mobile/src/main/rust/vpnhotspotd/src/shared/reporter/coalescer.rs),
  because its flush has to be part of the session's own result and bounded by its own writer
  queue. Dispatch is registry-first with no fall-through, so neither path can deliver into the
  other's. The app session finishes its reporter - flushing and joining it - before it drops
  the writer: finishing proves nothing more will be handed over, and joining the writer task
  afterwards proves what was handed over reached the peer.

The Kotlin side of the same subsystem lives under
[`mobile/src/main/java/be/mygod/vpnhotspot/root/daemon/`](../../mobile/src/main/java/be/mygod/vpnhotspot/root/daemon/).
Kotlin starts the binary, frames control messages, owns call IDs, and turns
daemon reports into app-visible exceptions or nonfatal warnings.

## Documents

- [`lifecycle.md`](lifecycle.md): daemon startup, control-loop ownership, call
  lifetime, session lifetime, cancellation, and shutdown.
- [`shizuku.md`](shizuku.md): proposed Shizuku-mode control plane,
  TestNetwork/TUN ownership, authenticated file-descriptor handoff, Rust
  dataplane, resource policy, and implementation and validation gates. Its
  security posture is best effort by design: device qualification proved that
  any local app can inject packets into the tunnel interface, so the mode
  protects tethered clients but not against other apps on the device.
- [`routing.md`](routing.md): desired routing state, reversible mutations,
  Clean behavior, and system mutation ownership.
- [`nat66.md`](nat66.md): IPv6 NAT runtime boundaries for TCP, UDP, ICMPv6,
  router advertisements, marks, and cleanup.
- [`dns.md`](dns.md): DNS listener ownership, resolver handoff, config snapshot
  semantics, and nonblocking assumptions.
- [`traffic.md`](traffic.md): MAC-facing traffic accounting, blocking scope,
  counter sources, recorder chain boundaries, and persistence mapping.
- [`errors.md`](errors.md): terminal errors, nonfatal reports, context/detail
  requirements, and background-task failure policy.
- [`invariants.md`](invariants.md): cross-module ownership, interception,
  cleanup, configuration, error, and platform-assumption rules.

## Maintenance Rule

Keep these docs in sync with daemon behavior. A change to
`mobile/src/main/rust/vpnhotspotd`, `mobile/src/main/proto/daemon.proto`, or
the Kotlin daemon controller should update `docs/vpnhotspotd` when it changes
internal ownership, lifecycle, cleanup, NAT66, DNS, routing, neighbour
monitoring, or error-reporting semantics.

Do not summarize away external side effects. If a change adds, removes, or
changes any mutation to kernel, netfilter, netd, resolver, socket, file
descriptor, process, or Android system state, the relevant doc must name:

- when the mutation happens;
- the exact external state or command shape;
- what owns rollback or normal stop cleanup;
- what Clean or process-death cleanup does, if the state can outlive the
  runtime;
- which missing-state or failure cases are expected.

For routing changes, [`routing.md`](routing.md) is a mutation catalog. Every
route, policy rule, address, iptables/ip6tables rule or chain, `ndc` request,
and Clean mutation must be listed there.

If a daemon-adjacent change does not affect those documented contracts, say so
in the change description or final response. Do not duplicate the protobuf
schema here; update `daemon.proto` comments if the wire-level contract itself
needs documentation.

Platform and compatibility assumptions that affect the public app contract stay
in the root [`README.md`](../../README.md), especially the `Other` and
`System/root command assumptions` sections. These daemon docs may link to those
assumptions, but should not create a second compatibility index.
