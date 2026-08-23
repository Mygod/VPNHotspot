//! Terminated TCP: the client's connection ends here, and a separate one is made upstream.
//!
//! Terminating rather than forwarding is forced, not chosen. Root mode lets the kernel do the TCP work through
//! `TPROXY`, which needs netfilter the app UID cannot touch, so the client-facing half is an in-process stack.
//! The consequence the design accepts is that packet-by-packet hop-limit transparency and TCP traceroute are
//! not promised: two independent connections cannot preserve one end-to-end IP header.
//!
//! Interception works by listening on the destination the client chose. A SYN is inspected, a socket is opened
//! listening on that exact endpoint, and only then is the packet handed to the stack - so the stack accepts a
//! connection to an address it does not own, which is what `any_ip` allows.
//!
//! **The client handshake is not held for the upstream connect; the two run concurrently.** The alternative -
//! holding the client's SYN until the upstream answers - would need the stack to defer a SYN it has already
//! been given, which it does not offer. So an unreachable destination is a reset after the handshake rather
//! than a timeout during it. That is the more informative failure of the two, and it is what every terminating
//! tunnel does.
//!
//! Concurrent means either order really happens. The connect starts from the SYN path, so a nearby remote that
//! greets first - SSH and SMTP both do - can have bytes waiting while the client half is still `SYN-RECEIVED`.
//! Those bytes are held rather than sent or dropped: [Engine::pump_to_client] consumes nothing from a half
//! that cannot yet send and requeues its readiness marker, and the client's own final ACK is what runs the
//! pump again. See [lifetime::handshaking] and the regressions beside it.

use std::net::SocketAddr;
use std::time::Instant;

use crate::workers::{Ended, Identity, Workers};
use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketStorage};
use smoltcp::socket::tcp::{Socket, State};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::icmp_nat::{nat66_hop_limit, Nat66HopLimit};
use vpnhotspotd::shared::model::Network;
use vpnhotspotd::shared::tcp_wire::Segment;

use std::collections::VecDeque;

use crate::flow_setup;
use vpnhotspotd::shared::admission::{
    largest_fitting, linear_footprint, Admission, Class, Denied, Lease, Request,
};
use vpnhotspotd::shared::dns_debt::Connection;
use vpnhotspotd::shared::fair::{self, FairQueue, FlowId, Progress};
use vpnhotspotd::shared::reply_bound::{built_depth, channel_footprint};

use crate::budget::FLOW_BUFFER;
use crate::output::Output;
use crate::owned::Owned;
use crate::report;
use crate::tcp_device::Shim;
use crate::tcp_dns::{self, Serving, Transactions};
use crate::tcp_flow::{self, Chunk, Event, Mailbox, READ_CHUNK};
use crate::tun_writer::Stamp;

mod dns;
mod lifetime;
mod terminal;

// The four endings and the one place a flow is given back, which is a decision of its own - see
// [crate::tcp::terminal].
pub(crate) use terminal::Finished;

/// How many chunks may be in flight from one upstream toward the stack at once.
///
/// One, in each direction and per flow. Anything larger buys nothing: the stack's own buffer is what absorbs
/// a burst, and a deeper channel would only move bytes out of the place that applies backpressure into one
/// that does not. Depth one *per flow* rather than one globally is the difference this tranche makes - a
/// single slot in front of every flow is head-of-line blocking, and the flow holding it is chosen by whichever
/// client stopped reading.
const FLOW_DEPTH: usize = 1;

/// What one entry in smoltcp's socket set really costs the engine: the storage slot the set holds inline,
/// beyond the two buffers the flow itself is charged for.
///
/// `SocketStorage` rather than a handle-sized proxy, and the difference is not small: the slot holds an
/// `Option<Item>` containing the socket enum itself, so a handle-sized figure understated it by roughly the
/// size of a TCP control block per prepared flow. Taken from the type, so it cannot drift from what smoltcp
/// actually allocates.
const SOCKET_SLOT_BYTES: u64 = std::mem::size_of::<SocketStorage<'static>>() as u64;

/// Every bounded channel one flow owns, at the message types and depth they are really built at.
///
/// Read from [channel_footprint] rather than approximated, because a channel is not free and there are five
/// of them: Tokio keeps a block of slots, a reused drained block, a semaphore, two waker slots and the
/// handles the owner retains. The earlier figure omitted all of it and called the difference padding.
///
/// Two of these carry [Owned] because two directions do: the client-to-upstream payload, and the filled query
/// travelling back to the owner that granted it.
fn flow_channels_footprint() -> Option<u64> {
    let depth = built_depth(FLOW_DEPTH);
    // The client-to-upstream payload channel.
    // One producer each, every one of them: these five are point to point between this flow's own worker and
    // its owner, and neither end clones its sender. So none of them can have a second sender in a grow race -
    // see [vpnhotspotd::shared::reply_bound::blocks_for].
    channel_footprint::<Owned>(depth, 1)?
        // This flow's depth-one mailbox toward the client's stack...
        .checked_add(channel_footprint::<Chunk>(depth, 1)?)?
        // ...and the consumption acknowledgment that frees it.
        .checked_add(channel_footprint::<()>(depth, 1)?)?
        // The owner's answers to this transport: a reservation's outcome, or a published query's.
        .checked_add(channel_footprint::<tcp_dns::Control>(depth, 1)?)?
        // ...and the exact filled query on its way back to that owner.
        .checked_add(channel_footprint::<Owned>(depth, 1)?)
}

/// What one flow really costs the aggregate: its stack buffers, the read scratch that is live at once, and
/// every channel it owns.
///
/// The two 64 KiB buffers dominate, which is the whole reason a flow is not "one record": counting it as one
/// would let memory run out long before descriptors did. Three chunk-sized terms sit beside them:
///
/// - the upstream half's persistent read buffer, allocated once per flow in [tcp_flow::splice];
/// - and *two* in the client-to-upstream direction, which is the peak that direction really reaches: the
///   splice takes a chunk out of the depth-one channel and can be blocked writing it while the engine has
///   already queued the next one behind it. Taking from a depth-one channel frees its permit, so both ends
///   being alive at once is ordinary rather than impossible - proved at the real peak in
///   [tests::an_ordinary_splices_payload_peak_is_what_its_flow_is_charged_for].
///
/// Three and not four, and the reason is the shape of [tcp_flow::splice] rather than the depth of a channel.
/// The upstream-to-client payload handed to the mailbox is a fourth buffer only if it can exist *while* those
/// two do, and it cannot: the splice is one `select!` and is therefore in exactly one branch at a time. While
/// it is blocked writing a taken chunk it is not reading, so there is no mailbox payload; while it is handing
/// a payload over it is not writing, so the taken chunk is already gone. The two directions peak
/// alternately, and the bound is the larger of them plus the read buffer.
///
/// An earlier version of this figure got the right number from the wrong argument - that the two ends of a
/// depth-one channel cannot both exist - and a fourth term before that was padding standing in for the
/// channel allocations nobody had counted, which are now counted above from the real types.
///
/// A DNS-over-TCP transport owns fewer payload chunks, not more: it has no upstream socket and therefore no
/// read buffer, and its framing holds two bytes of length prefix between reads rather than a buffer - the
/// query's own storage is admitted per query and charged to that query's debt, and so are the answer, its
/// framed copy and the piece in flight. What it does own is the same five channels, because both flow kinds
/// are built by one [flow_setup::prepare]. See [crate::tcp_dns].
///
/// Checked throughout: a figure that would wrap is a flow that cannot be accounted for and therefore must
/// not be built.
fn flow_footprint() -> Option<u64> {
    2u64.checked_mul(FLOW_BUFFER as u64)?
        .checked_add(3u64.checked_mul(READ_CHUNK as u64)?)?
        .checked_add(flow_channels_footprint()?)
}

/// Where a flow's upstream connection comes from.
///
/// The daemon has one: a socket opened on the selected network. A host cannot select a network, so this
/// crate's own tests hand the worker a gate that never opens instead - which is also the only way to prove
/// that an unwind really cancels a running task and joins it, rather than watching one end on its own
/// because the platform call failed. Nothing else about the flow changes.
#[derive(Clone)]
enum Upstreams {
    Platform,
    #[cfg(test)]
    Gated(std::sync::Arc<Gate>),
}

/// What a gated upstream records, so an unwind can be proved rather than assumed.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Gate {
    /// Tasks that reached the upstream wait.
    pub(crate) entered: std::sync::atomic::AtomicUsize,
    /// Tasks that left it, which for a wait that never completes means exactly: were cancelled.
    pub(crate) left: std::sync::atomic::AtomicUsize,
    /// Exactly what each worker was handed to connect on, in the order the workers reached the wait.
    ///
    /// The handle rather than a count of them, because the count is what a worker bound to the *previous*
    /// selection would also produce: a config's network reaches a flow through the engine's adopted upstream,
    /// and "a worker ran" says nothing about which network it ran against.
    pub(crate) networks: std::sync::Mutex<Vec<vpnhotspotd::shared::model::Network>>,
    /// A listener to connect to instead of parking forever, for the tests that need a remote which can
    /// finish sending first.
    ///
    /// `None` is the gate that never opens, which is what proves an unwind cancels a running task. `Some`
    /// makes the same connect the platform path makes, minus the one call a host cannot make - binding the
    /// socket to a `Network`. Without it the FIN-first half of the client-side state machine is unreachable
    /// here: only a real upstream can close before the client does.
    pub(crate) opens_onto: Option<SocketAddr>,
}

/// Increments the gate's departure count when the wait it stands for is dropped.
#[cfg(test)]
struct Waiting(std::sync::Arc<Gate>);

#[cfg(test)]
impl Drop for Waiting {
    fn drop(&mut self) {
        self.0
            .left
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Upstreams {
    async fn connect(
        &self,
        network: vpnhotspotd::shared::model::Network,
        destination: SocketAddr,
    ) -> Result<socket2::Socket, vpnhotspotd::shared::failure::Failure> {
        match self {
            Self::Platform => crate::egress::connect_tcp(network, destination).await,
            #[cfg(test)]
            Self::Gated(gate) => {
                gate.entered
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Recorded where the platform call would be made, so what is observed is the argument
                // `connect_tcp` would have received rather than the field a config was decoded into.
                gate.networks
                    .lock()
                    .expect("the gate's record is never held across an await")
                    .push(network);
                let Some(address) = gate.opens_onto else {
                    let _waiting = Waiting(gate.clone());
                    return std::future::pending().await;
                };
                let stream = tokio::net::TcpStream::connect(address)
                    .await
                    .map_err(vpnhotspotd::shared::failure::Failure::Expected)?;
                Ok(socket2::Socket::from(stream.into_std().map_err(
                    vpnhotspotd::shared::failure::Failure::Expected,
                )?))
            }
        }
    }
}

/// Where one flow's answers come from, which is what decides whether it needs a selected network at all.
///
/// Named rather than implied, and that is the correction: a virtual-DNS transport holds no socket bound to
/// the selection, so requiring one to *open* it refused a brand-new DNS connection for exactly as long as
/// the session had no upstream - which is the window a client is most likely to be resolving in. An ordinary
/// relayed flow genuinely cannot exist without one, because its whole purpose is a socket on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// A virtual-DNS transport. It terminates locally, owns no selected-network socket, and each of its
    /// queries is given the network current when the owner accepts *that query* - or answered with its own
    /// SERVFAIL while there is none.
    Resolver,
    /// An ordinary flow, which opens one upstream socket bound to this selected network.
    Upstream(Network),
}

/// What a flow carries, which decides what a config change does to it.
///
/// Recorded when the flow is opened rather than inferred from what it is doing, and that distinction is the
/// whole point: an idle DNS-over-TCP transport has no transaction outstanding, so a kind read from the
/// presence of one would call it an ordinary flow and reset it at the next generation - which is exactly the
/// client this design promises not to disturb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// An ordinary flow, holding one upstream socket bound to the selected network.
    Upstream,
    /// A virtual-DNS transport. It terminates locally, owns no selected-network socket, and its answers come
    /// from the platform resolver on whichever network was current when each query was accepted.
    Resolver,
}

impl Source {
    fn kind(self) -> Kind {
        match self {
            Self::Resolver => Kind::Resolver,
            Self::Upstream(_) => Kind::Upstream,
        }
    }

    /// Whether this flow's transport holds one logical resolver token for its whole life.
    fn resolver(self) -> bool {
        matches!(self, Self::Resolver)
    }
}

/// Which transports a retirement applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Retirement {
    /// Every flow. The epoch, because each flow is keyed by TUN-visible endpoints and a different device may
    /// be behind them now; and the session ending, which owes nobody anything.
    Everything,
    /// Only the flows that hold a selected-network socket. A generation change invalidates that socket and
    /// nothing else: a virtual-DNS transport has none, its queries are stamped one at a time at the owner's
    /// acceptance rather than fixed for the flow, and resetting it would end a stream the client is entitled
    /// to keep using - and lose the answer it is waiting for, which the handover does not cancel.
    Upstreams,
}

impl Retirement {
    fn retires(self, kind: Kind) -> bool {
        matches!(
            (self, kind),
            (Self::Everything, _) | (Self::Upstreams, Kind::Upstream)
        )
    }
}

/// What the client-side stack needs from a config, and what it does with each axis.
struct Flow {
    /// The TUN-visible endpoints, which with the family are the key the design specifies. The generation is not
    /// in it because it cannot vary within one table: either axis advancing retires all of this.
    client: SocketAddr,
    destination: SocketAddr,
    /// Which worker this flow's signals must name. Kept beside the handle because smoltcp reuses handles: a
    /// terminal or a readiness marker naming only a handle cannot be told apart from one belonging to the flow
    /// that reused it, and acting on the difference is a reset sent to the wrong client.
    worker: u64,
    /// What this flow carries, which is what a config change consults - never the transaction below.
    kind: Kind,
    /// Everything this flow owns: its record and upstream descriptor, its two stack buffers, its mailbox and
    /// its share of the fair queue - and, for a DNS-over-TCP flow, the one logical resolver token its
    /// transport holds for its whole life.
    ///
    /// Not an exchange's worth of bytes: those belong to the debt each submitted query takes, which is what
    /// lets them stay charged when this transport closes over a question still in flight. See
    /// [vpnhotspotd::shared::dns_debt].
    connection: Connection,
    /// This flow's depth-one payload mailbox, and the acknowledgment that frees it.
    chunks: mpsc::Receiver<Chunk>,
    consumed: mpsc::Sender<()>,
    /// This owner's half of the DNS control pair, the reservation this transport currently holds, the
    /// transaction it opened and the delivery parked for its answer - all of it built with the flow and
    /// charged with it. Present on every flow, because both kinds are built by one [flow_setup::prepare];
    /// an ordinary spliced flow simply never asks anything of it. See [crate::tcp_dns::Serving].
    serving: Serving,
    /// Dropped to signal the client's half-close upstream, so a request-then-response exchange still completes.
    downstream: Option<mpsc::Sender<Owned>>,
    /// Set once the remote has finished sending, so the client-side close is emitted exactly once.
    finished: bool,
    /// When this flow falls idle, from the phase its socket was last observed in. `None` only for
    /// `TIME-WAIT`, whose cleanup is smoltcp's own protocol timer rather than this owner's - see
    /// [crate::tcp::lifetime].
    deadline: Option<Instant>,
    /// Set once this flow's worker has run to completion cleanly and the client side has not finished yet.
    ///
    /// The state that keeps a terminating close honest. Both workers return as soon as *their* ordered work
    /// is done - the upstream half-close is written and the client's stack has taken the end of stream - and
    /// at that moment the client socket is typically still in `LAST-ACK`, `CLOSING` or `TIME-WAIT`, with a
    /// FIN to retransmit and a final acknowledgment to wait for. Removing the flow there took the client's
    /// half of the connection away mid-teardown.
    ///
    /// So a clean terminal *detaches* instead: the worker's descriptor and buffers are gone with its task,
    /// and what is left is a client-side-only flow that still owns its socket, its conservative grant and its
    /// DNS state until smoltcp reaches `Closed`, its outer floor runs out, a config retires it, or the
    /// session ends. No task of its own and no *per-flow* timer task stands behind it - its teardown is still
    /// scheduled, by the engine's combined stack-and-floor deadline, which is what lets the FIN be
    /// retransmitted. The ingress owner polls for it, exactly as it
    /// polls for a settled resolver transaction. See [Engine::detached] and [Engine::settled].
    detached: bool,
    /// Set once the client's handshake has completed.
    ///
    /// Load-bearing, not bookkeeping. Every "is this side done" question below is asked with `may_recv` and
    /// `may_send`, and both are false for a socket that is merely *listening* - so without this the first poll
    /// after opening a flow reads a brand-new connection as a half-closed one, drops the upstream write half,
    /// and the flow can never carry a byte.
    established: bool,
}

#[derive(Default)]
struct Counters {
    opened: u64,
    resolved: u64,
    /// Answered by this daemon instead of the platform: no selected network, or no descriptor for a
    /// transaction. Each is one query's own SERVFAIL on a stream that carries on.
    answered_here: u64,
    denied: u64,
    /// DNS-over-TCP transports a generation change left alone, because they hold no selected-network socket.
    preserved: u64,
    /// Refused because a prepared collection was full rather than because the aggregate was.
    unprepared: u64,
    /// An acknowledgment that could not be delivered, which means the flow is already on its way out.
    unacknowledged: u64,
    /// A token that could not be handed to the question it belonged to.
    unsettled: u64,
    /// Workers that finished cleanly while their client's teardown was still running, so the flow outlived
    /// its worker instead of being removed under a half-finished close.
    detached: u64,
    no_upstream: u64,
    reset: u64,
    /// Flows this owner took back because they had been idle past their phase's floor. Counted apart from
    /// [Counters::closed] because an expiry is a client-visible reset the client did not ask for, and apart
    /// from [Counters::reset] because a listening flow expires without one.
    expired: u64,
    closed: u64,
    to_upstream: u64,
    to_client: u64,
    stale: u64,
    unconsumed: u64,
}

impl Counters {
    fn describe(&self) -> String {
        format!(
            "opened {} resolved {} answered-here {} denied {} preserved {} no-upstream {} reset {} \
             expired {} detached {} closed {} to-upstream {} to-client {} stale {} unconsumed {}",
            self.opened,
            self.resolved,
            self.answered_here,
            self.denied,
            self.preserved,
            self.no_upstream,
            self.reset,
            self.expired,
            self.detached,
            self.closed,
            self.to_upstream,
            self.to_client,
            self.stale,
            self.unconsumed
        )
    }
}

pub(crate) struct Engine {
    interface: Interface,
    sockets: flow_setup::Sockets,
    device: Shim,
    /// Each flow beside the task that holds its upstream descriptor. A flow comes back out of here only once
    /// that task has run to completion, which is what the refunds below are keyed to.
    flows: Workers<SocketHandle, Flow>,
    /// The transactions DNS-over-TCP flows asked for, which outlive them. Kept apart from [Engine::flows]
    /// because a retirement joins that one and must not join this one - see [crate::tcp_dns].
    queries: Transactions,
    stamp: Stamp,
    /// The network flows connect out on. Only the handle: a terminated connection is `connect`ed, so the
    /// kernel picks the source and the reply arrives on that connection - there is no unconnected reply to
    /// check an interface index against, which is why the relays need one and this does not.
    upstream: Option<Network>,
    /// Cancelled for the whole table when a sweep runs, and replaced afterwards. A flow's own token says "this
    /// one is finished"; this one says "the network these are bound to is being left", which is the difference
    /// between an orderly upstream close and an abortive one.
    sweep: CancellationToken,
    /// Where a DNS-over-TCP transport asks for a query to be admitted. Through the owner rather than to a
    /// worker of its own, because admitting a resolver transaction is an accounting decision.
    asks: mpsc::Sender<tcp_dns::Ask>,
    /// Payload-free wakes from the flow tasks. Bounded by the number of live identities, with the fair queue
    /// coalescing duplicates, so a flood of wakes from one flow cannot grow it or displace another's.
    ready: mpsc::Sender<Event>,
    /// Whose turn it is and what each flow still owes the wire, per flow rather than globally.
    fair: FairQueue<SocketHandle, tcp_flow::Payload>,
    /// Explicit round-robin order for the *client to upstream* direction, for the same reason the fair queue
    /// is explicit for the other one: iterating a `HashMap` is an arbitrary order that changes when the map is
    /// resized, which is not fairness but a different unfairness each time.
    outgoing: VecDeque<SocketHandle>,
    /// The flow table, the fair queue, the socket set and the readiness channel: byte-only owners prepared at
    /// session start and charged once.
    tables: Lease,
    /// How many live flows every table above was prepared for. One number, so no two of them can disagree
    /// about what the engine may hold.
    prepared: usize,
    counters: Counters,
    /// The base for smoltcp's monotonic millisecond clock, which has to be the same instant for the whole
    /// session or its timers jump.
    started: Instant,
    /// Where every flow's upstream connection comes from.
    upstreams: Upstreams,
    /// Set by a test to make the next registration's admission refuse. Never true in a build that is not a
    /// test harness.
    #[cfg(test)]
    refuse_next_admission: bool,
    /// Where on the buffer clock the last closed flow's reservation became refundable, so a test can order it
    /// against the death of the query that reservation covered. Nothing at all outside a test harness - see
    /// [tcp_dns::Serving::close].
    #[cfg(test)]
    refundable_at: Option<u64>,
}

impl Engine {
    /// `seed` is this session's own, read from the kernel by [crate::tun_reader::prepare]. It is not
    /// optional and it is not derived here: smoltcp's `Config::random_seed` is zero by default and is what
    /// its RNG hands out passive-open initial sequence numbers from, so a default-configured interface gives
    /// every session of this daemon the *same* ISN sequence.
    ///
    /// What that costs is predictability across sessions, and the shape is worth stating exactly, because
    /// nothing of the previous session's own state survives it: the process exited, so its sockets and their
    /// TIME-WAIT timers are gone with it. What can still be in flight is what a *network* holds - segments of
    /// a connection this daemon terminated, delayed or retransmitted somewhere between here and the client -
    /// and those are matched by tuple and sequence alone. A successor that reopens the same client tuple and
    /// begins at the number its predecessor began at is therefore a connection whose window a stale segment
    /// can land in, with nothing left here to recognise it as old. Guessability is the same fact from the
    /// other side: an ISN sequence that restarts identically every session is one an on-path party can
    /// predict from having watched an earlier one.
    ///
    /// Taking it as a parameter is what keeps the entropy read at the session boundary, where a failure is a
    /// session that does not start rather than a fallback nobody notices.
    pub(crate) fn new(
        mtu: usize,
        seed: u64,
        admission: &mut Admission,
    ) -> Result<(Self, mpsc::Receiver<Event>, mpsc::Receiver<tcp_dns::Ask>), Denied> {
        let mut device = Shim::new(mtu);
        // No hardware address and no neighbour discovery: the TUN carries bare IP packets.
        let started = Instant::now();
        let mut config = Config::new(HardwareAddress::Ip);
        config.random_seed = seed;
        let mut interface = Interface::new(config, &mut device, SmolInstant::from_millis(0));
        // Accept packets addressed to somewhere else, which is the whole interception mechanism: the client is
        // talking to a remote host, not to this interface.
        interface.set_any_ip(true);
        interface.update_ip_addrs(|addresses| {
            // The addresses the session publishes. Client traffic arrives from inside these prefixes - IPv4
            // from Android's own inner NAT, IPv6 from the delegated /64 - so replies are on-link and need no
            // gateway, which a medium with no link layer could not resolve anyway.
            for cidr in [
                IpCidr::new(IpAddress::v4(192, 0, 2, 1), 30),
                IpCidr::new(IpAddress::v6(0x2001, 0xdb8, 1, 0, 0, 0, 0, 1), 64),
            ] {
                if addresses.push(cidr).is_err() {
                    report::message_with_details(
                        "shizuku.tcp_engine",
                        "the client-side stack refused one of its own addresses",
                        "InvalidInput",
                        [("address", cidr)],
                    );
                }
            }
        });
        // Derived rather than chosen, and derived from what *general* work may still take rather than from the
        // totals. "Total minus charged" counts the reserved floor as though a TCP flow could reach it, so a
        // bound taken that way inflates ordinary traffic's share with capacity protected for name resolution
        // and for packets already accepted. A constant would be wrong in both directions besides - preparing
        // tables for flows this device could never afford, or refusing flows it could carry.
        // The one figure the solver and every reservation read. A flow whose own footprint does not fit its
        // arithmetic is one no engine may prepare for, which is a refusal rather than a smaller bound.
        let per_flow = flow_footprint().ok_or(Denied::Arithmetic)?;
        let prepared = largest_fitting(admission.general_headroom(), per_flow, |flows| {
            tables_footprint(flows, mtu, admission.dns_token_cap())
        });
        // Charged before either channel exists, and charged for the depths that will exist: a queue built at
        // a minimum depth because the derived bound was zero is still a real allocation, and one nobody
        // charged for is the fail-open case the aggregate exists to prevent. [tables_footprint] is what both
        // the solver above and this reservation read, so the two cannot disagree about any of it.
        let bytes =
            tables_footprint(prepared, mtu, admission.dns_token_cap()).ok_or(Denied::Arithmetic)?;
        let tables = admission.reserve(Request::bytes(bytes, Class::General))?;
        // One marker slot per flow this engine is prepared for, which is exactly how many identities can be
        // ready at once: the fair queue keeps at most one marker per identity, so nothing beyond that can
        // reach here. At least one, because a zero-capacity channel is not constructible - and that minimum
        // is in the charge above rather than assumed free.
        let (ready, receiver) = mpsc::channel(flow_setup::ready_depth(prepared));
        // One outstanding ask per logical token, which is exactly how many transports can be asking at
        // once: a transport cannot exist without a token, and it asks one thing at a time - a length to
        // admit, the query that length was admitted for, or the delivery that answered it.
        let (asks, asking) = mpsc::channel(submission_depth(admission.dns_token_cap()));
        let queries = match Transactions::new(admission) {
            Ok(queries) => queries,
            Err(why) => {
                // Both halves of both channels go before the grant that covers them. Left to the unwind they
                // would drop *after* this release, which is the same fail-open moment as releasing while the
                // ingress task still held a receiver - see [Engine::release].
                drop(ready);
                drop(receiver);
                drop(asks);
                drop(asking);
                admission.release(tables);
                return Err(why);
            }
        };
        Ok((
            Self {
                interface,
                sockets: flow_setup::Sockets::new(prepared),
                device,
                flows: Workers::with_capacity("shizuku.tcp_flow", prepared),
                queries,
                stamp: Stamp::default(),
                upstream: None,
                sweep: CancellationToken::new(),
                asks,
                ready,
                fair: FairQueue::with_capacity(prepared),
                outgoing: VecDeque::with_capacity(prepared),
                tables,
                prepared,
                counters: Counters::default(),
                started,
                upstreams: Upstreams::Platform,
                #[cfg(test)]
                refuse_next_admission: false,
                #[cfg(test)]
                refundable_at: None,
            },
            receiver,
            asking,
        ))
    }

    /// Takes every flow's upstream from a gate rather than from the platform, which a host cannot reach.
    ///
    /// The engine is untouched: what changes is where one `await` inside each flow's own worker gets its
    /// socket from - and, because that gate never opens, what ends such a worker is the cancellation the
    /// unwind sends it.
    #[cfg(test)]
    pub(crate) fn upstreams_gated_by(&mut self, gate: std::sync::Arc<Gate>) {
        self.upstreams = Upstreams::Gated(gate);
    }

    /// Fills the worker table between the next registration's room check and its admission.
    ///
    /// One more flow, prepared and admitted by exactly the steps the registration under way uses, so what
    /// refuses that registration is [Workers::admit] finding the table genuinely full - with every resource
    /// the build took already in hand.
    #[cfg(test)]
    pub(crate) fn refuse_next_admission(&mut self) {
        self.refuse_next_admission = true;
    }

    /// Releases the engine's own capacity, after every flow and transaction has been settled.
    /// Gives this engine's own retained capacity back, once everything it covers is physically gone.
    ///
    /// The two receivers come in by value rather than outliving the call, for the same reason the UDP and Echo
    /// relays take theirs: the `tables` lease below covers the readiness and ask channels *whole* - shared
    /// state, blocks and the messages in their slots - so releasing while the ingress task still owned either
    /// receiving end would be capacity given back for allocations this process was still holding. Taking them
    /// by value is what makes that order structural instead of a comment; there is no drain, because dropping
    /// a receiver destroys whatever it had buffered and every sender is already gone by here.
    pub(crate) fn release(
        self,
        flows: mpsc::Receiver<Event>,
        asking: mpsc::Receiver<tcp_dns::Ask>,
        admission: &mut Admission,
    ) {
        // The transaction table's own lease first - it is a separate grant with its own contents.
        self.queries.release(admission);
        // Then everything `tables` pays for, before the grant goes - and it is worth reading this against
        // [tables_footprint] item by item, because that is the list this has to match: the worker table, the
        // fair queue, the round-robin order, both halves of the readiness channel, both halves of the ask
        // channel, the socket set, and the device's one MTU-sized output slot. The device was the one that
        // used to fall out of scope *after* the release, which is the same fail-open moment as a receiver
        // outliving the grant that covers it.
        drop(self.flows);
        drop(self.fair);
        drop(self.outgoing);
        drop(self.sockets);
        drop(self.ready);
        drop(flows);
        drop(self.asks);
        drop(asking);
        drop(self.device);
        admission.release(self.tables);
    }

    fn now(&self) -> SmolInstant {
        SmolInstant::from_micros(self.started.elapsed().as_micros() as i64)
    }

    /// Adopts a config, retiring by axis rather than wholesale.
    ///
    /// The two axes invalidate different things, and collapsing them was what made a DNS-over-TCP client's
    /// connection collateral damage of every handover:
    ///
    /// - the **epoch** retires everything, because every flow is keyed by TUN-visible endpoints and those may
    ///   name a different device now;
    /// - the **generation** retires exactly the flows that hold a socket bound to the network that changed.
    ///   A virtual-DNS transport holds none. It terminates locally, its answers come from the platform
    ///   resolver, and which network each of its queries went out on is fixed one query at a time when this
    ///   owner accepts it - so the transport itself is untouched, keeps its socket, its mailbox and its one
    ///   logical token, and the client goes on using the connection it opened.
    ///
    /// Neither axis cancels or awaits a resolver transaction: cancelling would return this process's
    /// descriptor and nothing of the platform's work, and awaiting one would make the config acknowledgement
    /// wait on a remote name server. A transaction that outlives its config settles into a SERVFAIL for its
    /// own query - see [Engine::settle].
    ///
    /// Returns once every task this retirement did cancel has been joined and its descriptor closed.
    pub(crate) async fn apply(
        &mut self,
        stamp: Stamp,
        upstream: Option<Network>,
        floor: usize,
        admission: &mut Admission,
        output: &mut Output,
    ) {
        let retiring = if stamp.epoch != self.stamp.epoch {
            Some(Retirement::Everything)
        } else if stamp.generation != self.stamp.generation {
            Some(Retirement::Upstreams)
        } else {
            None
        };
        // Adopted before the sweep, because whatever reset a swept flow owes its client is written during it:
        // the
        // writer gates a dequeued packet on the current retirement, so a reset stamped with the one being swept
        // would be purged along with the traffic it is meant to terminate. It is also what a query accepted
        // after this point is stamped with, which is the successor by construction.
        self.stamp = stamp;
        self.upstream = upstream;
        self.device.set_mtu(floor);
        if let Some(retiring) = retiring {
            self.retire(retiring, admission, output).await;
            // A cancelled token stays cancelled, so the successor generation's flows would close abortively for
            // no reason; every flow that read this one is gone by now - a preserved DNS transport never held
            // one, because it has no upstream socket to close abortively.
            self.sweep = CancellationToken::new();
        }
    }

    /// The whole-session path: retire every flow, then settle the transactions a handover would have left
    /// running.
    ///
    /// This is the one place a submitted transaction is cancelled, and it is not capacity being reclaimed -
    /// the session is over. Dropping a query returns this process's descriptor, which is as far as a process
    /// can get: the platform's own slot is released when its work finishes, and nothing here can observe or
    /// wait for that.
    pub(crate) async fn shutdown(&mut self, admission: &mut Admission, output: &mut Output) {
        self.retire(Retirement::Everything, admission, output).await;
        self.queries.shutdown(admission);
    }

    /// Sweeps the flows this retirement applies to: discard what their upstream halves were carrying, reset
    /// each client the stack has a remote endpoint for - one still listening, or already closed, goes in
    /// silence - then join each of their tasks so that every descriptor is actually gone before anything is
    /// refunded. A row a detached flow left behind has no task to join and is settled directly.
    ///
    /// Also called by the whole-session path above, which adds the one thing a handover must not do.
    async fn retire(&mut self, scope: Retirement, admission: &mut Admission, output: &mut Output) {
        // Cancelled before the flows' own tokens, because this is what each upstream half reads to decide
        // whether to close abortively, and a task that woke on its own token first would already be past it.
        // Only ordinary flows ever read it: a virtual-DNS transport has no upstream socket to linger on.
        self.sweep.cancel();
        {
            // Walked over the round-robin order rather than into a list of what is being retired. That order
            // is registered with every admitted flow and deregistered with every closed one, so it already
            // holds each live handle exactly once - see [vpnhotspotd::shared::fair::register] - and a list
            // built here would be scratch sized by traffic that no lease covers. Destructured because the
            // walk reads one field while the steps below write four others.
            let Engine {
                flows,
                sockets,
                fair,
                outgoing,
                counters,
                ..
            } = self;
            debug_assert_eq!(
                outgoing.len(),
                flows.len(),
                "the round-robin order indexes exactly the live flows"
            );
            for handle in outgoing.iter() {
                let Some(held) = flows.get_mut(handle) else {
                    continue;
                };
                if !scope.retires(held.record.kind) {
                    counters.preserved += 1;
                    continue;
                }
                // Already begun, by an idle expiry or by its own socket closing, and waiting only to be
                // joined. Everything below has been done to it once: its socket is aborted, whatever reset it
                // had a remote endpoint for is written - a socket still listening or already closed was
                // aborted in silence and counted none - and its worker is on its way out. Doing it again
                // would abort a closed socket and count a reset nothing sends. The wait afterwards still
                // covers it, because the descriptor it holds belongs to the generation this config is
                // leaving.
                if held.cancel.is_cancelled() {
                    continue;
                }
                // Discard before cancel, and per exact identity: a worker parked on an acknowledgment may
                // only be released once the owner has committed to dropping what that acknowledgment was
                // for. The reverse order is a task freed while the owner still believes it owes the bytes.
                drop(fair.begin_retire(identity(*handle, held.record.worker)));
                held.cancel.cancel();
                // dropped so a task blocked on the client's half of the splice wakes and exits
                held.record.downstream = None;
                // At most one terminal packet per retired flow, written before anything is freed, so a client
                // fails fast instead of waiting out its own retransmissions against a connection nothing will
                // answer again. Built here rather than at close, because close removes the socket that
                // carries it.
                let socket = sockets.get_mut::<Socket>(*handle);
                // Only a socket with a remote endpoint can be told: an eligible connected state builds and
                // counts one reset, while one still listening or already closed is aborted in silence -
                // there is nowhere for the stack to send it, and counting it would overstate what was sent.
                if socket.remote_endpoint().is_some() {
                    counters.reset += 1;
                }
                socket.abort();
            }
        }
        self.poll(output);
        // Settled here rather than waited for: a detached flow's worker finished long ago, so no terminal will
        // ever name it and the loop below would wait for one for ever. Found one at a time because settling
        // one removes it from the table this scan reads, and a sequence of single lookups allocates nothing.
        loop {
            let Some((handle, worker)) = self
                .flows
                .iter()
                .find(|(_, held)| held.record.detached && scope.retires(held.record.kind))
                .map(|(handle, held)| (*handle, held.record.worker))
            else {
                break;
            };
            self.settled(handle, worker, admission);
        }
        // Waited for by kind rather than by "nothing is running any more", because a preserved transport *is*
        // still running and must not be waited for: it holds no descriptor of the retired generation, and its
        // client is still using it. Over the live table rather than over what this call initiated, so a flow
        // an expiry had already cancelled is still joined and closed here - its descriptor is this
        // generation's, and an acknowledged config has to mean that descriptor is gone. Nothing can enter the
        // table while this awaits: the one owner that admits a flow is the task inside this call. Detached
        // rows are excluded because they were settled above; waiting for one is waiting for nothing.
        //
        // Events still in the channel are left there - a retired flow's belong to retired state and the
        // ordinary staleness check discards them when the caller next reads, while a preserved flow's are
        // still its own. A task parked on that channel wakes on its own token rather than on this drain.
        while self
            .flows
            .values()
            .any(|held| scope.retires(held.record.kind) && !held.record.detached)
        {
            let terminal = self.flows.finished().await;
            self.close(terminal, admission, output);
        }
        // An orphaned socket is structurally impossible rather than swept up here, and this is what says so.
        // A socket is added by [flow_setup::prepare] with the flow that owns it and removed by
        // [Engine::reclaim] with that same flow, which is the only path out of the table and is reached only
        // after the exact identity has been validated - so a slot no live flow holds would be a bug in that
        // pairing, not residue for a retirement to tidy. The loop this replaces allocated a list sized by the
        // socket set to find, in every real case, nothing.
        debug_assert_eq!(
            self.sockets.iter().count(),
            self.flows.len(),
            "every socket belongs to exactly one live flow"
        );
    }

    /// Hands one TCP packet to the stack, opening a flow first when this is a SYN for a destination with none.
    ///
    /// `resolver` says the destination is a virtual address on port 53, which the caller has already classified;
    /// such a flow is answered by the platform resolver rather than by an upstream connection.
    ///
    /// `now` is the ingress task's own reading of the clock for this packet, and it is what the flow's outer
    /// idle lifetime is measured from - see [crate::tcp::lifetime].
    pub(crate) fn accept(
        &mut self,
        packet: &[u8],
        segment: Segment,
        resolver: bool,
        now: Instant,
        output: &mut Output,
        admission: &mut Admission,
    ) {
        if segment.syn
            && !self.flows.values().any(|flow| {
                flow.record.client == segment.source
                    && flow.record.destination == segment.destination
            })
        {
            // A duplicate SYN for an existing flow falls through to the stack instead, which reuses the
            // half-open state it already has rather than allocating a second flow.
            if !self.open(
                segment.source,
                segment.destination,
                segment.hop_limit,
                resolver,
                now,
                admission,
            ) {
                return;
            }
        }
        if !self.device.push(packet) {
            // The stack still holds an untaken packet, which means the poll that should have consumed it did
            // not run. Counted rather than queued, because a queue here would hide the bug.
            self.counters.unconsumed += 1;
            return;
        }
        self.poll(output);
        // Resolved after the poll rather than before it, and by the tuple this packet actually named: the
        // flow may have been opened above, and the phase the packet puts it into is the state its socket ends
        // up in. A packet naming no flow at all - the stack answered it with a reset, or dropped it - rearms
        // nothing, which is the whole reason this is a lookup rather than a field carried down from above.
        let handled = self
            .flows
            .iter()
            .find(|(_, held)| {
                held.record.client == segment.source
                    && held.record.destination == segment.destination
            })
            .map(|(handle, held)| (*handle, held.record.worker));
        if let Some((handle, worker)) = handled {
            self.rearm(handle, worker, now);
        }
    }

    /// Opens one flow, if what it is can exist right now.
    ///
    /// The selected network is required by *what the flow is*, not by the fact that it is TCP. An ordinary
    /// relayed flow opens a socket on the selection and cannot exist without one. A virtual-DNS transport
    /// holds no such socket: it is admitted with none, its first questions are answered with their own
    /// SERVFAIL, and the same stream then resolves normally once a successor config supplies a network - which
    /// is the whole point of a transport that survives generation.
    fn open(
        &mut self,
        client: SocketAddr,
        destination: SocketAddr,
        hop_limit: u8,
        resolver: bool,
        now: Instant,
        admission: &mut Admission,
    ) -> bool {
        let source = if resolver {
            Source::Resolver
        } else {
            let Some(upstream) = self.upstream else {
                self.counters.no_upstream += 1;
                return false;
            };
            Source::Upstream(upstream)
        };
        // The remaining hop limit is validated before the connect, and an expired one is not connected at all:
        // a terminated flow cannot preserve it, but it can refuse to launder a packet that should have died.
        let Nat66HopLimit::Forward(hop_limit) = nat66_hop_limit(Some(hop_limit)) else {
            return false;
        };
        // Destructured so the transaction below can borrow the pieces it needs disjointly: the fair queue and
        // the round-robin order are what it registers into, and everything else is what its operations build.
        #[cfg(test)]
        let refuse = std::mem::take(&mut self.refuse_next_admission);
        let Engine {
            sockets,
            flows,
            fair,
            outgoing,
            prepared,
            ready,
            asks,
            sweep,
            counters,
            upstreams,
            ..
        } = self;
        let mut ops = Admit {
            sockets,
            flows,
            ready,
            asks,
            sweep,
            counters,
            admission,
            client,
            destination,
            hop_limit,
            source,
            now,
            upstreams: upstreams.clone(),
            #[cfg(test)]
            refuse,
        };
        // One transaction, and production's own: capacity in both tables before a descriptor is opened or a
        // byte charged, then build, then register both collections together, then admit - with every failure
        // after the build unwinding the socket, the channels and the grant. See
        // [vpnhotspotd::shared::fair::admit_flow].
        match fair::admit_flow(&mut ops, fair, outgoing, *prepared) {
            Ok(_) => {
                self.counters.opened += 1;
                if resolver {
                    self.counters.resolved += 1;
                }
                true
            }
            Err(fair::Refused::AtCapacity(_)) => {
                self.counters.unprepared += 1;
                false
            }
            // Already counted and, where it deserved one, reported by the operation that failed.
            Err(fair::Refused::Unbuildable(())) => false,
        }
    }

    /// One payload-free wake. The marker says an identity *may* have work; what to do about it is decided
    /// from owner state, never from the wake.
    ///
    /// `admitting` is the session's current admission state rather than the one the flow was opened under.
    /// A `STOPPING` session may drain what it already owns - the payload below still reaches the client -
    /// but it may not refresh a lifetime, because refreshing one is tracking state a session that is
    /// stopping has said it will not track.
    pub(crate) fn handle(
        &mut self,
        event: Event,
        admitting: bool,
        now: Instant,
        output: &mut Output,
    ) {
        let Event { handle, worker } = event;
        // Both halves validated together. A marker naming a handle whose flow has been replaced belongs to
        // the predecessor, and acting on it would put the successor's mailbox into the round.
        if !self.flows.current(&handle, worker) {
            self.counters.stale += 1;
            return;
        }
        let id = identity(handle, worker);
        // Whether this wake carried anything at all. A marker whose mailbox is empty, whose payload the fair
        // owner refused, or that names an identity already retiring is not activity: it is this daemon
        // talking to itself, and it must not hold a client's connection open.
        let mut carried = false;
        // Depth one, so there is at most one to take; the loop is what makes that a fact rather than an
        // assumption.
        while let Ok(chunk) = self
            .flows
            .get_mut(&handle)
            .map_or(Err(mpsc::error::TryRecvError::Disconnected), |flow| {
                flow.record.chunks.try_recv()
            })
        {
            match chunk {
                Chunk::Payload(bytes) => {
                    if let Err((bytes, _)) = self.fair.deliver(id, bytes) {
                        // The mailbox was occupied or the identity is retiring. Either way the bytes are this
                        // owner's to drop, and dropping them is the only honest thing: the producer is
                        // waiting for an acknowledgment it will get from cancellation instead.
                        drop(bytes);
                        self.counters.stale += 1;
                    } else {
                        carried = true;
                    }
                }
                // Ordered strictly after whatever payload is already in the mailbox.
                Chunk::Finished => {
                    self.fair.signal_eof(id);
                    if let Some(flow) = self.flows.get_mut(&handle) {
                        flow.record.finished = true;
                    }
                    carried = true;
                }
            }
        }
        self.fair.mark_ready(id);
        self.poll(output);
        // After the poll, because the end of stream taken above is what makes this owner close its own half
        // and the floor that applies is the one the socket lands on. A DNS-over-TCP answer arrives here as
        // ordinary payload and counts exactly as much as an upstream's bytes do.
        if carried && admitting {
            self.rearm(handle, worker, now);
        }
    }

    /// Runs the stack until it has nothing more to do, then moves bytes in both directions. Called after
    /// anything that could have changed a socket, because smoltcp is a state machine that only advances when
    /// polled.
    pub(crate) fn poll(&mut self, output: &mut Output) {
        loop {
            let progressed = self
                .interface
                .poll(self.now(), &mut self.device, &mut self.sockets);
            // One segment per poll, because the device holds one: draining it and polling again is what lets
            // the stack produce the next, and having emitted one is itself progress - the stack refused to
            // transmit while the slot was full, so this loop must come round again even if nothing else
            // changed.
            let emitted = match self.device.drain() {
                Some(packet) => {
                    output.packet(self.stamp, packet);
                    true
                }
                None => false,
            };
            let moved = self.pump();
            if matches!(progressed, PollResult::None) && !moved && !emitted {
                break;
            }
        }
        // A socket the stack has finished with outlives its flow only until here, and *every* closed socket
        // counts - what a `Closed` socket means is that the stack will never touch it again, which is as true
        // of one that never opened as of one that did.
        //
        // Walked over the round-robin order rather than into a list of handles: that order already holds each
        // live handle exactly once, so a list would be scratch proportional to live flows that no lease
        // covers, allocated on the busiest path in the engine. The three borrows below are of separate
        // fields, which is what lets the walk read one while the steps write the others.
        let Engine {
            flows,
            sockets,
            fair,
            outgoing,
            ..
        } = self;
        debug_assert_eq!(
            outgoing.len(),
            flows.len(),
            "the round-robin order indexes exactly the live flows"
        );
        for handle in outgoing.iter() {
            let Some(held) = flows.get_mut(handle) else {
                continue;
            };
            if sockets.get::<Socket>(*handle).state() != State::Closed {
                continue;
            }
            // Discarded before anything is cancelled or dropped, and per exact identity. A worker parked on an
            // acknowledgment may only be released once the owner has committed to dropping what that wait was
            // for; cancelling first frees the task while the owner still believes it owes those bytes, and
            // dropping the downstream endpoint first does the same to the other direction.
            drop(fair.begin_retire(identity(*handle, held.record.worker)));
            // The client half is done. An attached flow's worker is told to stop and the release follows
            // joining it, so the descriptor is gone before the accounting says so; a detached flow has no
            // worker left, and the ingress owner's own scan is what settles it - see [Engine::detached].
            held.cancel.cancel();
            held.record.downstream = None;
        }
    }

    /// Moves bytes between the stack and the flow tasks. Returns whether anything moved, so the caller knows
    /// to poll again.
    fn pump(&mut self) -> bool {
        // Upstream to client first, because a full stack send buffer is what throttles the remote.
        let mut moved = self.pump_to_client();
        moved |= self.pump_to_upstream();
        moved
    }

    /// One fair round toward the clients: every flow that was ready when the round began is offered its turn
    /// before any flow gets a second one.
    ///
    /// The budget is taken at the start, so a flow that rotates to the back after a short write does not
    /// extend the round, and a flow signalling readiness in a tight loop cannot make the round its own. A flow
    /// whose client has stopped reading keeps its exact offset and its turn passes to the next one; nothing
    /// about it reaches any other flow.
    fn pump_to_client(&mut self) -> bool {
        let mut moved = false;
        let mut round = self.fair.begin_round();
        while let Some(id) = self.fair.next(&mut round) {
            // Before anything is consumed, and for payload and ordered end of stream alike: a client half
            // that has not finished handshaking cannot take bytes yet, and taking the marker away from one is
            // how a payload gets stranded. The worker that produced it is waiting for an acknowledgment
            // before it reads again, so nothing else would ever wake this flow - the marker *is* the only
            // thing that brings it back. So it goes back, nothing is consumed, and `moved` deliberately stays
            // as it was: reporting progress here would spin [Engine::poll] on a socket that cannot make any.
            // The client's own final ACK is what runs this again.
            //
            // A half whose send side is *over* is the opposite case and falls through to the send below,
            // where the error leaves it on the retirement path - see [lifetime::handshaking].
            if self
                .socket(id.handle)
                .is_some_and(|socket| lifetime::handshaking(socket.state()))
            {
                self.fair.mark_ready(id);
                continue;
            }
            let Some(pending) = self.fair.peek(id) else {
                // Nothing owed but a marker, or an ordered end of stream with no payload before it. Both are
                // settled by acknowledging with nothing sent.
                if matches!(self.fair.serviced(id, 0), Progress::Eof) {
                    self.acknowledge(id);
                    moved = true;
                }
                continue;
            };
            // Capped at the read quantum in this direction too, so one flow's chunk cannot be an arbitrarily
            // long turn: what does not go now keeps its exact offset and comes back next round.
            let offered = &pending[..pending.len().min(READ_CHUNK)];
            // The socket is reached through the fields rather than through [Engine::socket], because the
            // pending slice above borrows the fair queue and a `&mut self` helper would take the whole owner
            // with it. Written out, the two borrows are of different fields and the payload never has to be
            // copied to satisfy them.
            if !self.flows.contains(&id.handle) {
                self.counters.stale += 1;
                continue;
            }
            let sent = match self
                .sockets
                .get_mut::<Socket>(id.handle)
                .send_slice(offered)
            {
                Ok(sent) => sent,
                // The client half is gone; the flow's own close follows, and nothing here has to force it.
                Err(_) => {
                    moved = true;
                    continue;
                }
            };
            if sent > 0 {
                moved = true;
                self.counters.to_client += sent as u64;
            }
            match self.fair.serviced(id, sent) {
                // The whole chunk went. Only now may its producer read another - which is what makes depth
                // one mean depth one rather than "one in the mailbox and one being written".
                Progress::Consumed | Progress::Eof => {
                    self.acknowledge(id);
                    moved = true;
                }
                // Still owed. The offset is exact and the flow is already back in the order.
                Progress::Blocked | Progress::Idle => {}
            }
        }
        moved
    }

    /// Tells one flow's producer that its chunk has been consumed.
    ///
    /// Depth one and never awaited: the producer takes the previous acknowledgment before it reads again, so
    /// the slot is free. A full slot or a gone receiver both mean the flow is on its way out, which its own
    /// terminal settles.
    fn acknowledge(&mut self, id: FlowId<SocketHandle>) {
        if let Some(flow) = self.flows.get(&id.handle) {
            if flow.record.worker == id.worker && flow.record.consumed.try_send(()).is_err() {
                self.counters.unacknowledged += 1;
            }
        }
    }

    /// One fair round toward the upstreams, in the same explicit order and with the same quantum.
    ///
    /// A `HashMap` iteration would be an arbitrary order that changes when the map is resized, which is not
    /// fairness but a different unfairness each time - and the quantum is what stops one flow with a full
    /// 64 KiB receive buffer from being the whole round.
    fn pump_to_upstream(&mut self) -> bool {
        let mut moved = false;
        for _ in 0..self.outgoing.len() {
            let Some(handle) = self.outgoing.pop_front() else {
                break;
            };
            self.outgoing.push_back(handle);
            // Only ever read as much as the flow task can take right now: leaving the rest in the stack's
            // receive buffer is what closes the client's window instead of buffering here.
            let room = self
                .flows
                .get(&handle)
                .and_then(|flow| flow.record.downstream.as_ref())
                .map_or(0, |downstream| downstream.capacity());
            let finished = self
                .flows
                .get(&handle)
                .is_some_and(|flow| flow.record.finished);
            // Read from the *phase* rather than from one state, because a third-handshake ACK that also
            // carries FIN opens and half-closes this connection in a single step and `Established` is never
            // observable for it - see [lifetime::opened]. Watching for that one state left such a flow
            // believing it had never opened, so the half-close below was never propagated and its upstream
            // peer waited for bytes nobody would send.
            if self
                .socket(handle)
                .is_some_and(|socket| lifetime::opened(socket.state()))
            {
                if let Some(flow) = self.flows.get_mut(&handle) {
                    flow.record.established = true;
                }
            }
            let Some(socket) = self.socket(handle) else {
                continue;
            };
            if finished && socket.may_send() {
                // the remote finished sending, so the client is told the same way rather than reset
                socket.close();
                moved = true;
            }
            if room == 0 || !socket.can_recv() {
                continue;
            }
            // Bounded by the quantum rather than by whatever the receive buffer holds: a whole 64 KiB copied
            // into a depth-one channel is one flow's turn lasting as long as it likes, and the rest stays
            // where it is until this flow comes round again.
            let mut chunk = Vec::new();
            if socket
                .recv(|data| {
                    let taken = data.len().min(READ_CHUNK);
                    // Exactly what was taken, so the buffer this flow is charged for is the size it really
                    // holds rather than whatever an amortised growth happened to reserve.
                    chunk = data[..taken].to_vec();
                    (taken, ())
                })
                .is_err()
                || chunk.is_empty()
            {
                continue;
            }
            self.counters.to_upstream += chunk.len() as u64;
            // Counted from here to wherever it is dropped - queued, taken by the flow task, or discarded with
            // a receiver that is gone - because that whole span is one of the chunk-sized terms in
            // [flow_footprint].
            let chunk = Owned::new(chunk);
            moved = true;
            if let Some(downstream) = self
                .flows
                .get(&handle)
                .and_then(|flow| flow.record.downstream.as_ref())
            {
                if downstream.try_send(chunk).is_err() {
                    // capacity was checked immediately above and this engine is the only producer, so a
                    // failure here means the receiver is gone and the flow's close is already on its way
                    self.counters.stale += 1;
                }
            }
        }
        // A client that half-closed stops the upstream write half, which the task sees as its channel
        // closing. Walked over the round-robin order in place rather than collected: the order already holds
        // each live handle exactly once, and a list here would be scratch proportional to live flows that no
        // lease covers, on the path every poll takes.
        let Engine {
            flows,
            sockets,
            outgoing,
            ..
        } = self;
        for handle in outgoing.iter() {
            let Some(held) = flows.get_mut(handle) else {
                continue;
            };
            if held.record.established
                && held.record.downstream.is_some()
                && !sockets.get::<Socket>(*handle).may_recv()
            {
                held.record.downstream = None;
                moved = true;
            }
        }
        moved
    }

    fn socket(&mut self, handle: SocketHandle) -> Option<&mut Socket<'static>> {
        self.flows
            .contains(&handle)
            .then(|| self.sockets.get_mut::<Socket>(handle))
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "{} flows, {}, {}",
            self.flows.len(),
            self.queries.describe(),
            self.counters.describe()
        )
    }
}

/// Every retained table the engine holds for `flows` live flows, plus its one fixed output slot.
///
/// One function rather than a sum written out at the construction site, because the derived bound is solved
/// against exactly this and the reservation is taken from exactly this: two spellings of it could disagree,
/// and the disagreement would be a table prepared for more than was charged.
/// The engine's own flow-admission operations, as [fair::admit_flow] drives them.
///
/// A borrowed view rather than the whole engine, so the transaction can hold the fair queue and the
/// round-robin order at the same time as the tables these touch.
struct Admit<'a> {
    sockets: &'a mut flow_setup::Sockets,
    flows: &'a mut Workers<SocketHandle, Flow>,
    ready: &'a mpsc::Sender<Event>,
    asks: &'a mpsc::Sender<tcp_dns::Ask>,
    sweep: &'a CancellationToken,
    counters: &'a mut Counters,
    admission: &'a mut Admission,
    client: SocketAddr,
    destination: SocketAddr,
    hop_limit: u8,
    /// Where this flow's answers come from, which is also the only thing that decides whether a selected
    /// network was needed to open it at all.
    source: Source,
    /// When this flow is being opened, which is where its first idle deadline is measured from.
    now: Instant,
    upstreams: Upstreams,
    /// Arms the worker table to refuse this registration's admission. See
    /// [Engine::refuse_next_admission].
    #[cfg(test)]
    refuse: bool,
}

/// What one built-but-not-yet-admitted flow holds: the record the worker table will take, and the pieces the
/// worker itself needs. Kept together so an unwind drops all of them.
struct Built {
    flow: Flow,
    identity: Identity,
    /// Taken by the worker future when one is started. Absent afterwards, so an unwind on the admission path
    /// drops what is left rather than pretending to hold what the future already owns.
    mailbox: Option<Mailbox>,
    receiver: Option<mpsc::Receiver<Owned>>,
    /// The transport's halves of the DNS control pair, on the same terms.
    control: Option<mpsc::Receiver<tcp_dns::Control>>,
    filled: Option<mpsc::Sender<Owned>>,
}

impl Built {
    /// What is left after a worker future has taken the pieces it owns, so an unwind drops what remains
    /// rather than pretending to hold what the future already has.
    fn without_worker(flow: Flow, identity: Identity) -> Self {
        Self {
            flow,
            identity,
            mailbox: None,
            receiver: None,
            control: None,
            filled: None,
        }
    }
}

impl Admit<'_> {
    /// Takes everything one flow needs, in the order that lets each step undo the ones before it.
    ///
    /// `Denied::Grant` covers the arithmetic too: a per-flow footprint that does not fit its own `u64` is a
    /// flow that cannot be charged, so nothing is built for it.
    fn prepare(&mut self) -> Result<flow_setup::Prepared, flow_setup::Denied> {
        let bytes = flow_footprint().ok_or(flow_setup::Denied::Grant)?;
        flow_setup::prepare(
            self.admission,
            self.sockets,
            self.flows,
            self.ready,
            flow_setup::Sizing {
                bytes,
                buffer: FLOW_BUFFER,
                depth: FLOW_DEPTH,
                hop_limit: self.hop_limit,
                resolver: self.source.resolver(),
            },
            IpListenEndpoint::from(endpoint(self.destination)),
        )
    }

    /// Turns prepared resources into the record the worker table will take, and the pieces its worker needs.
    fn assemble(&self, prepared: flow_setup::Prepared) -> (SocketHandle, u64, Built) {
        let flow_setup::Prepared {
            connection,
            handle,
            identity,
            mailbox,
            incoming,
            consumed,
            downstream,
            receiver,
            serving,
            control,
            filled,
        } = prepared;
        let worker = identity.id;
        (
            handle,
            worker,
            Built {
                flow: Flow {
                    client: self.client,
                    destination: self.destination,
                    worker,
                    // Decided by the classification the caller already made, and recorded rather than
                    // rediscovered: what this flow *is* cannot be read off what it happens to be doing.
                    kind: self.source.kind(),
                    connection,
                    chunks: incoming,
                    consumed,
                    serving,
                    downstream: Some(downstream),
                    finished: false,
                    detached: false,
                    // Set before the flow is admitted, so a SYN whose stack path never runs - the device
                    // still held an untaken packet - is a flow this owner will take back rather than one
                    // that never got a deadline at all. Read off the socket that was just built rather than
                    // assumed to be listening, so the opening floor comes from the same table every later
                    // phase does.
                    deadline: lifetime::deadline(
                        self.now,
                        self.sockets.get::<Socket>(handle).state(),
                    ),
                    established: false,
                },
                identity,
                mailbox: Some(mailbox),
                receiver: Some(receiver),
                control: Some(control),
                filled: Some(filled),
            },
        )
    }
}

impl fair::FlowOps for Admit<'_> {
    type Handle = SocketHandle;
    type Record = Built;
    /// Already counted and reported where it deserved one, so there is nothing left to say about it.
    type Error = ();
    type Payload = tcp_flow::Payload;

    fn has_room(&self) -> bool {
        self.flows.has_room()
    }

    fn build(&mut self) -> Result<(SocketHandle, u64, Built), ()> {
        // Every resource, taken in the one order where each step can undo the ones before it - see
        // [flow_setup::prepare], which owns that order and is the only thing that takes any of them.
        let prepared = match self.prepare() {
            Ok(prepared) => prepared,
            Err(denied) => {
                if let flow_setup::Denied::Listen(e) = denied {
                    report::message_with_details(
                        "shizuku.tcp_listen",
                        format!("the client-side stack cannot intercept this destination: {e}"),
                        "InvalidInput",
                        [("client", self.client), ("destination", self.destination)],
                    );
                }
                self.counters.denied += 1;
                return Err(());
            }
        };
        let (handle, worker, built) = self.assemble(prepared);
        // Armed by a test, and only here: after the room check this registration already passed, after its
        // socket, grant, identity and channels exist, and before the unchanged admission below. Nothing is
        // created for it and nothing returns early - the worker table itself answers.
        #[cfg(test)]
        if std::mem::take(&mut self.refuse) {
            self.flows.refuse_next_admit();
        }
        Ok((handle, worker, built))
    }

    fn unwind(&mut self, handle: SocketHandle, _worker: u64, record: Built) {
        let Built {
            flow,
            mailbox,
            receiver,
            control,
            filled,
            ..
        } = record;
        let Flow {
            connection,
            chunks,
            consumed,
            serving,
            downstream,
            ..
        } = flow;
        // Unreachable for a flow that was never admitted; ended anyway, because the alternative is a grant
        // nothing releases. Before the release below, so nothing is given back while its bytes still exist.
        // The transaction it answers with is nothing here: a flow that was never admitted has none.
        let _ = serving.close(self.admission);
        // The reverse of the preparation, written once and where the preparation is - see
        // [flow_setup::release].
        flow_setup::release(
            self.admission,
            self.sockets,
            handle,
            flow_setup::Leftovers {
                connection,
                mailbox,
                receiver,
                incoming: chunks,
                consumed,
                downstream,
                control,
                filled,
            },
        );
    }

    fn admit(&mut self, handle: SocketHandle, _worker: u64, record: Built) -> Result<(), Built> {
        self.start(handle, record)
    }
}

impl Admit<'_> {
    /// Records the flow and starts its worker, which is the last step and the only one that can still refuse.
    // The record comes back by value on refusal because the caller is what unwinds it, and what it is
    // holding by then is a socket, a lease and five channels. Boxing it would put an allocation on the one
    // path that runs because the daemon is already out of room.
    #[allow(clippy::result_large_err)]
    fn start(&mut self, handle: SocketHandle, record: Built) -> Result<(), Built> {
        let Built {
            flow,
            identity,
            mailbox,
            receiver,
            control,
            filled,
        } = record;
        let (Some(mailbox), Some(receiver), Some(control), Some(filled)) =
            (mailbox, receiver, control, filled)
        else {
            // Unreachable: only this function takes them, and it takes them once.
            return Err(Built::without_worker(flow, identity));
        };
        let token = identity.cancel.clone();
        let Source::Upstream(upstream) = self.source else {
            // No transaction is started here: an idle DNS-over-TCP connection owes its one logical token and
            // nothing else, and it needs no selected network to exist. The transport asks ingress when it
            // actually has a question - see [Engine::ask].
            let asks = self.asks.clone();
            return self
                .flows
                .admit(
                    handle,
                    &identity,
                    flow,
                    tcp_dns::serve(mailbox, receiver, asks, control, filled, token),
                )
                // The worker future took the mailbox, the receiver and both control halves, so what comes
                // back cannot carry them - and does not need to: they are dropped with the future that was
                // never spawned.
                .map_err(|(flow, _)| Built::without_worker(flow, identity));
        };
        // An ordinary flow's transport never reads these, and dropping them here is what says so: the
        // channels themselves are charged with the flow either way, because one preparation builds both
        // kinds.
        drop(control);
        drop(filled);
        let upstreams = self.upstreams.clone();
        let destination = self.destination;
        let sweep = self.sweep.clone();
        self.flows
            .admit(handle, &identity, flow, async move {
                // The connect is inside the cancellation rather than ahead of it. An unanswered SYN is
                // bounded only by the kernel's own connect timeout, so a sweep that had to wait for one would
                // stall the retirement it must finish before the session may acknowledge the new generation.
                let connected = tokio::select! {
                    biased;
                    () = token.cancelled() => return Ended::Expected,
                    connected = upstreams.connect(upstream, destination) => connected,
                };
                match connected {
                    Ok(socket) => match tokio::net::TcpStream::from_std(socket.into()) {
                        Ok(stream) => {
                            tcp_flow::splice(stream, mailbox, receiver, token, sweep).await
                        }
                        // The socket is dropped here, which closes it: there is no stream to adopt it, so
                        // nothing else could.
                        Err(e) => Ended::Failed {
                            context: "shizuku.tcp_upstream_adopt",
                            error: e,
                        },
                    },
                    // Classified rather than collapsed. An unreachable or refused destination is the ordinary
                    // case and the client learns of it from the reset the engine writes when this terminal
                    // arrives - never from a report per attempt, since a client chooses how many attempts
                    // there are. A socket this daemon could not create, bind to the selected network or
                    // register is its own failure and is reported as one.
                    Err(failure) => failure.ended("upstream connect"),
                }
            })
            .map_err(|(flow, _)| Built::without_worker(flow, identity))
    }
}

fn tables_footprint(flows: usize, mtu: usize, tokens: u32) -> Option<u64> {
    Workers::<SocketHandle, Flow>::footprint(flows)?
        .checked_add(FairQueue::<SocketHandle, tcp_flow::Payload>::footprint(
            flows,
        )?)?
        .checked_add(linear_footprint(
            flows,
            std::mem::size_of::<SocketHandle>() as u64,
        )?)?
        // The readiness channel, at the depth that will really be built - including the minimum a zero-flow
        // engine still allocates.
        // Fan-in: every flow clones the readiness sender, and every DNS transport clones the ask sender, so
        // both carry one producer per prepared flow.
        .checked_add(channel_footprint::<Event>(
            flow_setup::ready_depth(flows),
            flows,
        )?)?
        // The channel a transport asks its owner on, at the type it really carries: every variant of
        // [tcp_dns::Ask], not the one shape a query used to travel in.
        .checked_add(channel_footprint::<tcp_dns::Ask>(
            submission_depth(tokens),
            flows,
        )?)?
        // smoltcp's socket set, at its real slot layout.
        .checked_add(linear_footprint(flows, SOCKET_SLOT_BYTES)?)?
        // The device's one output slot, which is fixed engine scratch rather than anything a flow owns.
        .checked_add(mtu as u64)
}

/// How deep the channel a transport asks its owner on is built. One per logical token, and at least one for
/// the same reason: a
/// zero-capacity channel is not constructible, and that minimum is charged rather than assumed free.
fn submission_depth(tokens: u32) -> usize {
    built_depth(tokens as usize)
}

/// The pair that actually names a flow. A smoltcp handle is a slot the stack reuses, so a signal carrying one
/// alone cannot be told apart from one belonging to whatever reused it - and the difference between those two
/// is a reset delivered to the wrong client.
fn identity(handle: SocketHandle, worker: u64) -> FlowId<SocketHandle> {
    FlowId::new(handle, worker)
}

fn endpoint(address: SocketAddr) -> IpEndpoint {
    IpEndpoint::new(address.ip().into(), address.port())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::Arc;

    use smoltcp::phy::ChecksumCapabilities;
    use smoltcp::wire::{
        IpProtocol, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr, TcpSeqNumber,
    };

    use super::*;
    use crate::owned;
    use crate::tun_writer;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use vpnhotspotd::shared::admission::Totals;
    use vpnhotspotd::shared::dns_wire;

    pub(crate) const MTU: usize = 1_500;
    /// Anything the client-side stack will accept as a listen endpoint; what is under test is the
    /// registration around it rather than the interception rule.
    pub(crate) const DESTINATION: SocketAddr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 7), 443));
    pub(crate) const RESOLVER: SocketAddr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 53), 53));

    pub(crate) fn client(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 2), port))
    }

    /// One IPv4 TCP segment as a client puts it on the TUN.
    ///
    /// Assembled through smoltcp's own wire types rather than by hand, because the engine's device
    /// advertises full checksum capabilities: a segment built any other way is discarded before it reaches
    /// the state machine, and a test would be proving that the checksum is checked.
    pub(crate) fn segment(source: SocketAddr, destination: SocketAddr, repr: TcpRepr) -> Vec<u8> {
        let (SocketAddr::V4(source), SocketAddr::V4(destination)) = (source, destination) else {
            panic!("this harness speaks the family Android's inner NAT hands the daemon");
        };
        let ip = Ipv4Repr {
            src_addr: *source.ip(),
            dst_addr: *destination.ip(),
            next_header: IpProtocol::Tcp,
            payload_len: repr.buffer_len(),
            hop_limit: 64,
        };
        let mut bytes = vec![0u8; ip.buffer_len() + ip.payload_len];
        let mut packet = Ipv4Packet::new_unchecked(&mut bytes[..]);
        ip.emit(&mut packet, &ChecksumCapabilities::default());
        repr.emit(
            &mut TcpPacket::new_unchecked(packet.payload_mut()),
            &IpAddress::Ipv4(ip.src_addr),
            &IpAddress::Ipv4(ip.dst_addr),
            &ChecksumCapabilities::default(),
        );
        bytes
    }

    /// What one segment the engine produced says, read back with the checksums a client would verify.
    pub(crate) struct Wire {
        pub(crate) control: TcpControl,
        pub(crate) source: SocketAddr,
        pub(crate) destination: SocketAddr,
        /// Where the client's own acknowledgment goes next, which is this segment's sequence plus whatever
        /// of the sequence space it consumed.
        pub(crate) acknowledging: TcpSeqNumber,
        pub(crate) payload: Vec<u8>,
    }

    pub(crate) fn parse(bytes: &[u8]) -> Wire {
        let packet = Ipv4Packet::new_checked(bytes).expect("an IPv4 packet");
        let source = IpAddress::Ipv4(packet.src_addr());
        let destination = IpAddress::Ipv4(packet.dst_addr());
        let tcp = TcpPacket::new_checked(packet.payload()).expect("a TCP segment");
        let repr = TcpRepr::parse(
            &tcp,
            &source,
            &destination,
            &ChecksumCapabilities::default(),
        )
        .expect("with the checksum the client would verify");
        Wire {
            control: repr.control,
            source: SocketAddr::new(packet.src_addr().into(), repr.src_port),
            destination: SocketAddr::new(packet.dst_addr().into(), repr.dst_port),
            acknowledging: repr.seq_number + repr.segment_len(),
            payload: repr.payload.to_vec(),
        }
    }

    /// A session's aggregate, sized so the engine's derived bound comes out at `flows`.
    ///
    /// Solved rather than asserted: the bound is `largest_fitting` over what *general* work may still take,
    /// so the total below is searched for the smallest that admits exactly the wanted number. A constant
    /// would drift the moment any footprint changed.
    pub(crate) fn admission_for(flows: usize) -> Admission {
        // The smallest total this fixture can even be constructed at, found rather than guessed.
        let mut floor = 1u64 << 12;
        while totals(floor).is_none() {
            floor *= 2;
        }
        let wanted = flows.max(1);
        let (mut low, mut high) = (floor, 1u64 << 30);
        while low < high {
            let mid = low + (high - low) / 2;
            if affords(mid) >= wanted {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        // A zero-flow engine is one byte short of affording its first flow rather than an arbitrary small
        // number, so what it proves is about the boundary.
        let total = if flows == 0 { low - 1 } else { low };
        assert!(total >= floor, "no constructible total affords no flow");
        assert_eq!(
            affords(total),
            flows,
            "the fixture must produce exactly the bound under test"
        );
        totals(total).expect("constructible")
    }

    /// How many flows an engine on this byte total would prepare for - the engine's own solver, not a second
    /// one.
    fn affords(byte_total: u64) -> usize {
        let Some(admission) = totals(byte_total) else {
            return 0;
        };
        largest_fitting(
            admission.general_headroom(),
            flow_footprint().expect("bounded"),
            |n| tables_footprint(n, MTU, admission.dns_token_cap()),
        )
    }

    fn totals(byte_total: u64) -> Option<Admission> {
        Admission::new(Totals {
            admission_id: 1,
            record_total: 256,
            dns_record_floor: 8,
            byte_total,
            reserved_byte_floor: 1 << 16,
            fragment_cap: 1 << 16,
            dns_token_cap: 4,
            byte_only_owners: 8,
        })
        .ok()
    }

    /// Everything a session hands the engine, built the way a session builds it.
    ///
    /// Shared with [crate::tcp::dns]'s own tests, which drive the same engine through the DNS owner rather
    /// than through the splice - one fixture, so the two cannot disagree about what a session is.
    pub(crate) struct Session {
        pub(crate) engine: Engine,
        pub(crate) output: Output,
        pub(crate) markers: mpsc::Receiver<Event>,
        pub(crate) asks: mpsc::Receiver<tcp_dns::Ask>,
        /// The writer's own receiving end. Held so the writer stays connected, and read by the tests that
        /// are about the packets this engine actually produced.
        pub(crate) queue: tun_writer::Queue,
    }

    impl Session {
        /// Releases the engine's retained capacity the way the ingress task does: with this session's own
        /// halves of the readiness and ask channels handed in, so they die before the grant covering them.
        pub(crate) fn release(self, admission: &mut Admission) {
            self.engine.release(self.markers, self.asks, admission);
        }
    }

    /// One fixed seed, so the packets a test reads back are reproducible. Production reads a fresh one from
    /// the kernel for every session - see [crate::tun_reader::prepare] - and what that changes is the initial
    /// sequence numbers, which no test below asserts an absolute value for.
    pub(crate) const SEED: u64 = 0x5eed_5eed_5eed_5eed;

    pub(crate) async fn session(admission: &mut Admission, gate: &Arc<Gate>) -> Session {
        seeded_session(admission, gate, SEED).await
    }

    async fn seeded_session(admission: &mut Admission, gate: &Arc<Gate>, seed: u64) -> Session {
        let (engine, markers, asks) = Engine::new(MTU, seed, admission).expect("an engine");
        let (writer, queue, _terminals) = tun_writer::channel();
        let mut session = Session {
            engine,
            output: Output::testing(MTU, 8, writer),
            markers,
            asks,
            queue,
        };
        // The one thing a host cannot do, and the only thing injected: every flow's upstream is a gate that
        // never opens, so a worker ends when - and only when - the daemon cancels it.
        session.engine.upstreams_gated_by(gate.clone());
        // The real config path, so the upstream the flows below open against is one the engine adopted
        // rather than one a test wrote into it. The stamp is unchanged, so nothing is retired.
        session
            .engine
            .apply(
                Stamp::default(),
                Some(1),
                MTU,
                admission,
                &mut session.output,
            )
            .await;
        session
    }

    /// One ordinary query for `example.com`, which is what a client's stream carries - and what a SERVFAIL
    /// has to echo to be an answer at all.
    pub(super) fn question(id: u16) -> Vec<u8> {
        let mut query = vec![
            0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 7, b'e', b'x',
            b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0x00, 0x01, 0x00, 0x01,
        ];
        query[..2].copy_from_slice(&id.to_be_bytes());
        query
    }

    /// What the client was told, in the only two fields a refusal has to get right: whose question this
    /// answers, and that it failed.
    pub(super) fn servfail_for(answer: &[u8], question: &[u8]) {
        assert_eq!(&answer[..2], &question[..2], "that query's own identifier");
        assert_eq!(answer[3] & 0x0f, 2, "SERVFAIL");
        assert_eq!(
            &answer[12..],
            &question[12..],
            "with the question echoed back"
        );
    }

    /// Everything the engine prepared once, in the collections whose capacity the byte charge is *made of*.
    ///
    /// Capacities, and only capacities. A live count says nothing about what is allocated behind it, a
    /// handle names a slot rather than counting them, and a configured bound is what the engine *intended*
    /// rather than what it took - so none of the three is in here.
    ///
    /// Nor are the two hash maps, and that is the deliberate omission. `Vec`, `VecDeque` and the channels
    /// below are charged for their capacity and document what that capacity means, so watching it is watching
    /// the charge. `HashMap::capacity` is a documented *lower* bound on what still fits and carries no promise
    /// across a removal, so a before-and-after comparison of it would be reading the allocator through a
    /// number that cannot report one. What bounds those maps is the gate in [Workers::admits] and
    /// [FairQueue::admit], and their live counts are asserted beside every use of this.
    #[derive(Debug, PartialEq, Eq)]
    struct Retained {
        outgoing: usize,
        /// Slots the socket set's backing vector really holds - see [flow_setup::Sockets].
        slots: usize,
        /// Free slots, not the constant maximum: a marker left behind by a refused registration shows up
        /// here and nowhere else.
        ready_free: usize,
        ready_max: usize,
        asks_max: usize,
    }

    impl Retained {
        fn of(engine: &Engine) -> Self {
            Self {
                outgoing: engine.outgoing.capacity(),
                slots: engine.sockets.slots,
                ready_free: engine.ready.capacity(),
                ready_max: engine.ready.max_capacity(),
                asks_max: engine.asks.max_capacity(),
            }
        }
    }

    /// N flows fit through the engine's own registration, and N+1 is refused before a socket, a lease, a
    /// channel or a task exists.
    #[tokio::test]
    async fn n_flows_fit_and_the_next_is_refused_before_any_resource() {
        let prepared = 4usize;
        let mut admission = admission_for(prepared);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut session = session(&mut admission, &gate).await;
        let empty = Retained::of(&session.engine);
        assert_eq!(session.engine.prepared, prepared);
        let idle = admission.bytes_charged();

        for flow in 0..prepared {
            assert!(
                session.engine.open(
                    client(10_000 + flow as u16),
                    DESTINATION,
                    64,
                    false,
                    Instant::now(),
                    &mut admission
                ),
                "flow {flow} must fit"
            );
        }
        // Every worker is really running, parked on its upstream.
        tokio::task::yield_now().await;
        assert_eq!(gate.entered.load(Relaxed), prepared, "one task each");
        assert_eq!(gate.left.load(Relaxed), 0);
        assert_eq!(session.engine.flows.len(), prepared);
        assert_eq!(session.engine.fair.len(), prepared);
        assert_eq!(session.engine.counters.opened, prepared as u64);
        assert_eq!(
            admission.bytes_charged(),
            idle + prepared as u64 * flow_footprint().expect("bounded")
        );
        let full = Retained::of(&session.engine);
        assert_eq!(
            full,
            Retained {
                slots: prepared,
                ..empty
            },
            "one slot per flow, and nothing else the engine prepared grew"
        );
        assert_eq!(
            session.engine.sockets.iter().count(),
            prepared,
            "one real socket each"
        );
        let charged = admission.bytes_charged();
        let records = admission.records_charged();

        // One past the bound. Refused before a socket is added, a lease taken or a task started.
        assert!(!session.engine.open(
            client(20_000),
            DESTINATION,
            64,
            false,
            Instant::now(),
            &mut admission
        ));
        assert_eq!(
            session.engine.counters.unprepared, 1,
            "counted as the table"
        );
        assert_eq!(session.engine.counters.denied, 0, "not as the aggregate");
        assert_eq!(session.engine.counters.opened, prepared as u64);
        assert_eq!(gate.entered.load(Relaxed), prepared, "no task was started");
        assert_eq!(admission.bytes_charged(), charged, "no lease was taken");
        assert_eq!(admission.records_charged(), records);
        assert_eq!(
            Retained::of(&session.engine),
            full,
            "and not one slot, capacity or marker moved"
        );

        // Swept the way a session sweeps, so every task is cancelled, joined and closed.
        session
            .engine
            .shutdown(&mut admission, &mut session.output)
            .await;
        assert_eq!(gate.left.load(Relaxed), prepared, "each one joined");
        assert_eq!(session.engine.flows.len(), 0);
        assert_eq!(admission.bytes_charged(), idle);
        assert_eq!(session.engine.sockets.iter().count(), 0);
        assert_eq!(
            Retained::of(&session.engine),
            full,
            "an emptied engine keeps exactly what it took, and gives none of it back early"
        );
        session.release(&mut admission);
        assert_eq!(
            admission.bytes_charged(),
            baseline,
            "and the tables with it"
        );
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A flow the aggregate cannot grant is refused before its socket, its identity or any of its five
    /// channels exists.
    ///
    /// The order [flow_setup::prepare] takes is grant, socket, identity, channels - so what proves the
    /// channels were never built is that the two steps *before* them left no trace either. Neither is
    /// inferred: the socket set reports the slots its backing vector really holds, and the identity cursor is
    /// read by opening the next flow and seeing that it got the number the refused one would have taken.
    ///
    /// This is the fail-open shape the per-flow footprint closed. The five channels are real allocations
    /// Tokio makes after the grant, and a refusal that built them anyway would be heap nothing agreed to -
    /// on the one path that runs because the daemon is already out of room.
    #[tokio::test]
    async fn a_flow_the_aggregate_cannot_grant_builds_nothing_at_all() {
        let mut admission = admission_for(4);
        let gate = Arc::new(Gate::default());
        let mut session = session(&mut admission, &gate).await;
        let idle = Retained::of(&session.engine);
        assert_eq!(idle.slots, 0, "no socket has ever been added");

        // Everything but a sliver of what general work may take belongs to something else, so one flow's
        // composite grant cannot be met.
        let headroom = admission.general_headroom().bytes;
        let held = admission
            .reserve(Request::bytes(
                headroom - flow_footprint().expect("bounded") / 2,
                Class::General,
            ))
            .expect("most of what general work may take");
        let charged = admission.bytes_charged();
        let records = admission.records_charged();

        assert!(
            !session.engine.open(
                client(10_700),
                DESTINATION,
                64,
                false,
                Instant::now(),
                &mut admission
            ),
            "a flow whose grant cannot be met must not exist"
        );
        assert_eq!(session.engine.counters.denied, 1, "and is counted");
        assert_eq!(
            admission.bytes_charged(),
            charged,
            "a refused flow charges nothing"
        );
        assert_eq!(admission.records_charged(), records);
        assert_eq!(
            Retained::of(&session.engine),
            idle,
            "and nothing the engine prepared was touched"
        );
        assert_eq!(
            session.engine.sockets.slots, 0,
            "no socket was added, which is the step *before* the channels"
        );
        assert_eq!(session.engine.flows.len(), 0);

        // Room again, and the next flow takes worker zero - so the refused one consumed no identity either,
        // and the channels that are built after an identity cannot have been built for it.
        admission.release(held);
        assert!(session.engine.open(
            client(10_701),
            DESTINATION,
            64,
            false,
            Instant::now(),
            &mut admission
        ));
        let handle = *session.engine.flows.keys().next().expect("the flow");
        assert_eq!(
            session
                .engine
                .flows
                .get(&handle)
                .expect("held")
                .record
                .worker,
            0,
            "the refused registration issued no identity"
        );

        session
            .engine
            .shutdown(&mut admission, &mut session.output)
            .await;
        session.release(&mut admission);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// The channel term covers every channel a real preparation builds, at the depth those channels were
    /// really built at.
    ///
    /// Read off the endpoints [flow_setup::prepare] produced rather than restated: a channel dropped from the
    /// term, or one built deeper than it is charged for, is exactly the fail-open shape the old figure had -
    /// heap Tokio allocates after the grant, standing behind a chunk-sized constant that was called padding.
    #[tokio::test]
    async fn the_channel_term_covers_every_channel_a_flow_really_builds() {
        let mut admission = admission_for(4);
        let mut sockets = flow_setup::Sockets::new(4);
        let mut flows = Workers::<SocketHandle, Flow>::with_capacity("test", 4);
        let (ready, _markers) = mpsc::channel(4);
        let prepared = flow_setup::prepare(
            &mut admission,
            &mut sockets,
            &mut flows,
            &ready,
            flow_setup::Sizing {
                bytes: flow_footprint().expect("bounded"),
                buffer: FLOW_BUFFER,
                depth: FLOW_DEPTH,
                hop_limit: 64,
                resolver: true,
            },
            IpListenEndpoint::from(endpoint(RESOLVER)),
        )
        .expect("a prepared flow");

        // Every endpoint the preparation handed back, asked what it was really built at.
        let built = channel_footprint::<Owned>(prepared.downstream.max_capacity(), 1)
            .and_then(|bytes| {
                bytes.checked_add(channel_footprint::<Chunk>(
                    prepared.mailbox.chunks.max_capacity(),
                    1,
                )?)
            })
            .and_then(|bytes| {
                bytes.checked_add(channel_footprint::<()>(
                    prepared.consumed.max_capacity(),
                    1,
                )?)
            })
            .and_then(|bytes| {
                bytes.checked_add(channel_footprint::<tcp_dns::Control>(
                    prepared.control.max_capacity(),
                    1,
                )?)
            })
            .and_then(|bytes| {
                bytes.checked_add(channel_footprint::<Owned>(
                    prepared.filled.max_capacity(),
                    1,
                )?)
            })
            .expect("bounded");
        assert_eq!(
            flow_channels_footprint().expect("bounded"),
            built,
            "five channels, and the term is charged for exactly those five"
        );

        let flow_setup::Prepared {
            connection,
            handle,
            mailbox,
            incoming,
            consumed,
            downstream,
            receiver,
            serving,
            control,
            filled,
            ..
        } = prepared;
        let _ = serving.close(&mut admission);
        flow_setup::release(
            &mut admission,
            &mut sockets,
            handle,
            flow_setup::Leftovers {
                connection,
                mailbox: Some(mailbox),
                receiver: Some(receiver),
                incoming,
                consumed,
                downstream: Some(downstream),
                control: Some(control),
                filled: Some(filled),
            },
        );
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// The payload term of the per-flow footprint is the peak a real splice reaches, measured rather than
    /// restated - and it is three chunks, not four.
    ///
    /// Four was arithmetic about a task that cannot exist. [tcp_flow::splice] is one `select!` loop, so it is
    /// either writing a client-to-upstream chunk *or* handing a payload to its mailbox and waiting for the
    /// acknowledgment; it is never doing both. What can be alive at once is therefore the persistent read
    /// buffer it allocates once, plus whichever of those two directions the task is in, plus the one chunk the
    /// engine may have queued behind it in the depth-one channel. Three.
    ///
    /// The fourth term was standing in for heap nobody had counted: five bounded channels the flow really
    /// owns. Those are counted now, from their own production types, and the assertion below reads both
    /// production figures rather than restating either - the payload term has to equal what this run
    /// observed.
    #[tokio::test]
    async fn an_ordinary_splices_payload_peak_is_what_its_flow_is_charged_for() {
        owned::reset();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener");
        let address = listener.local_addr().expect("bound");
        let upstream = tokio::net::TcpStream::connect(address)
            .await
            .expect("a loopback connection");
        let (mut peer, _) = listener.accept().await.expect("the other half");

        let depth = built_depth(FLOW_DEPTH);
        let (chunks, mut incoming) = mpsc::channel(depth);
        let (ready, mut markers) = mpsc::channel(4);
        let (consumed, acknowledged) = mpsc::channel(depth);
        let (downstream, receiver) = mpsc::channel::<Owned>(depth);
        let identity = crate::mailbox::Marker {
            handle: smoltcp::iface::SocketHandle::default(),
            worker: 3,
        };
        let mailbox = Mailbox {
            chunks,
            ready,
            consumed: acknowledged,
            identity,
        };
        let cancel = CancellationToken::new();
        let sweep = CancellationToken::new();
        let token = cancel.clone();
        let spliced = tokio::spawn(tcp_flow::splice(upstream, mailbox, receiver, token, sweep));

        // The peer never reads, so the client-to-upstream direction backs up: the task ends up holding one
        // chunk inside its write while the engine's depth-one channel holds the next. Filled through the
        // channel's own capacity rather than by a failed send, because a chunk built for a send that is
        // refused would be a third allocation this test made.
        let mut queued = 0usize;
        while downstream.capacity() > 0 && queued < 20_000 {
            downstream
                .send(Owned::new(vec![0x5au8; READ_CHUNK]))
                .await
                .expect("the splice is reading");
            queued += 1;
            tokio::task::yield_now().await;
        }
        assert!(
            downstream.capacity() == 0,
            "the write half never backed up: {queued} chunks went through"
        );
        let backed_up = owned::peak().0.buffers;
        assert_eq!(
            backed_up, 2,
            "one chunk inside the write, one queued behind it"
        );

        // While that write is blocked the task cannot be in its read arm, which is exactly why a fourth chunk
        // is not reachable: the remote sends and nothing appears in the mailbox.
        peer.write_all(&[0x11u8; READ_CHUNK])
            .await
            .expect("the remote may write");
        tokio::task::yield_now().await;
        assert!(
            incoming.try_recv().is_err(),
            "a task blocked writing is not also producing a payload"
        );
        assert_eq!(
            owned::peak().0.buffers,
            2,
            "and nothing was allocated for the payload it is not building"
        );

        // Drain the peer, which unblocks the write and lets the read arm run. Now the live buffers are the
        // mailbox payload and whatever the engine queued behind it - still two.
        let drained = tokio::spawn(async move {
            let mut sink = vec![0u8; 1 << 16];
            let mut taken = 0usize;
            while taken < queued * READ_CHUNK {
                match peer.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => taken += read,
                }
            }
            peer
        });
        let chunk = incoming.recv().await.expect("the remote's bytes arrive");
        let Chunk::Payload(payload) = chunk else {
            panic!("only payload is handed over here")
        };
        assert_eq!(payload.len(), READ_CHUNK);
        assert_eq!(markers.recv().await, Some(identity));
        let peak = owned::peak().1.buffers;
        drop(payload);
        consumed.send(()).await.expect("the producer waits");

        cancel.cancel();
        assert!(spliced.await.is_ok(), "the splice finished");
        let mut peer = drained.await.expect("the drain finished");
        peer.shutdown().await.ok();

        assert_eq!(
            peak, 2,
            "at most two counted buffers ever existed at once, in either direction"
        );
        // The third is the read buffer [tcp_flow::splice] allocates once per flow, which is heap rather than
        // an owned payload and so is named here rather than counted above.
        let payload_term =
            flow_footprint().expect("bounded") - flow_channels_footprint().expect("bounded");
        assert_eq!(
            payload_term,
            (2 * FLOW_BUFFER + (peak + 1) * READ_CHUNK) as u64,
            "the flow is charged for exactly the peak it can reach, plus its two stack buffers"
        );
        assert!(
            payload_term < (2 * FLOW_BUFFER + 4 * READ_CHUNK) as u64,
            "and not for a fourth chunk standing in for channels that are counted now"
        );
    }

    /// The worker table refuses *after* the flow has been built, and production's own unwind gives every
    /// resource back.
    ///
    /// The refusal is the table's own. A test arms [Workers::refuse_next_admit] from inside the registration
    /// under way - after its room check has passed and after its socket, grant, identity and channels exist -
    /// so [Workers::admit] answers with its ordinary typed refusal, carrying back the record it was handed,
    /// with everything the build took still in hand. Nothing returns early, nothing extra is built, and no
    /// record or socket is orphaned to manufacture it.
    #[tokio::test]
    async fn a_worker_table_refusal_after_the_build_unwinds_every_resource() {
        let prepared = 4usize;
        let mut admission = admission_for(prepared);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut session = session(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        let seeded = prepared - 1;
        for flow in 0..seeded {
            assert!(session.engine.open(
                client(10_000 + flow as u16),
                DESTINATION,
                64,
                false,
                Instant::now(),
                &mut admission
            ));
        }
        tokio::task::yield_now().await;
        assert_eq!(gate.entered.load(Relaxed), seeded, "one task each");
        let before = Retained::of(&session.engine);
        assert_eq!(before.slots, seeded, "one slot each, and no more");
        let charged = admission.bytes_charged();
        let records = admission.records_charged();

        session.engine.refuse_next_admission();
        assert!(!session.engine.open(
            client(20_000),
            DESTINATION,
            64,
            false,
            Instant::now(),
            &mut admission
        ));
        tokio::task::yield_now().await;

        assert_eq!(
            session.engine.counters.unprepared, 1,
            "the table refused, and after the build"
        );
        assert_eq!(
            session.engine.flows.len(),
            seeded,
            "the refused registration was not admitted"
        );
        assert_eq!(
            gate.entered.load(Relaxed),
            seeded,
            "and never spawned a task"
        );
        assert_eq!(gate.left.load(Relaxed), 0, "nor ended one");
        assert_eq!(
            session.engine.fair.len(),
            seeded,
            "it was taken back out of the fair queue"
        );
        assert_eq!(session.engine.outgoing.len(), seeded);
        assert_eq!(
            session.engine.sockets.iter().count(),
            seeded,
            "the socket its build added left the set again"
        );
        assert_eq!(
            admission.bytes_charged(),
            charged,
            "and its grant went back, exactly once"
        );
        assert_eq!(admission.records_charged(), records);
        // The build really did take a slot - every existing one was occupied - and the unwind really did
        // free it. What may never happen is the backing vector growing past what the engine was charged
        // for, which is what a second flow built beside this one would have done.
        let after = Retained::of(&session.engine);
        assert_eq!(
            after,
            Retained {
                slots: seeded + 1,
                ..before
            },
            "nothing but the one slot its own build took, and no capacity moved"
        );
        assert!(
            after.slots <= session.engine.prepared,
            "the socket set grew past the {} slots it was charged for",
            session.engine.prepared
        );
        assert_eq!(
            after.ready_free, after.ready_max,
            "and no readiness marker was left behind"
        );
        assert!(
            session.markers.try_recv().is_err(),
            "nothing was published for a flow that does not exist"
        );

        // The slot it borrowed is genuinely free again: the next flow takes it without the set growing.
        assert!(session.engine.open(
            client(20_001),
            DESTINATION,
            64,
            false,
            Instant::now(),
            &mut admission
        ));
        tokio::task::yield_now().await;
        assert_eq!(
            Retained::of(&session.engine),
            after,
            "the refused build's slot was reused rather than added to"
        );
        assert_eq!(gate.entered.load(Relaxed), prepared);

        // Every task the engine really started is cancelled and joined by the sweep.
        session
            .engine
            .shutdown(&mut admission, &mut session.output)
            .await;
        assert_eq!(gate.left.load(Relaxed), prepared, "each one joined");
        assert_eq!(session.engine.flows.len(), 0);
        assert_eq!(admission.bytes_charged(), idle);
        assert_eq!(Retained::of(&session.engine), after);
        session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// The fair queue refuses *after* the flow has been built, and production's own unwind gives every
    /// resource back.
    ///
    /// The engine's pre-check reads its own bound; the queue refuses against its own. A real queue prepared
    /// for fewer flows than the engine is what separates the two.
    #[tokio::test]
    async fn a_fair_queue_refusal_after_the_build_unwinds_every_resource() {
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut session = session(&mut admission, &gate).await;
        let idle = admission.bytes_charged();
        session.engine.fair = FairQueue::with_capacity(0);
        let before = Retained::of(&session.engine);

        assert!(!session.engine.open(
            client(10_003),
            DESTINATION,
            64,
            false,
            Instant::now(),
            &mut admission
        ));
        tokio::task::yield_now().await;
        assert_eq!(session.engine.counters.unprepared, 1);
        assert_eq!(session.engine.flows.len(), 0, "no record was published");
        assert_eq!(gate.entered.load(Relaxed), 0, "and no task was started");
        assert_eq!(session.engine.fair.len(), 0);
        assert!(session.engine.outgoing.is_empty(), "nor half-registered");
        assert_eq!(admission.bytes_charged(), idle, "the grant went back, once");
        assert_eq!(
            session.engine.sockets.iter().count(),
            0,
            "the socket the build added left the set again"
        );
        let after = Retained::of(&session.engine);
        assert_eq!(
            after,
            Retained { slots: 1, ..before },
            "nothing but the one slot its own build took, and no capacity moved"
        );
        assert!(
            after.slots <= session.engine.prepared,
            "the socket set grew past the {} slots it was charged for",
            session.engine.prepared
        );
        assert_eq!(after.ready_free, after.ready_max, "no marker left behind");
        assert!(session.markers.try_recv().is_err());

        session
            .engine
            .shutdown(&mut admission, &mut session.output)
            .await;
        session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// The session's seed reaches the initial sequence numbers its stack hands out.
    ///
    /// What this closes is a real predictability, not a style point. smoltcp's `Config::random_seed` is zero
    /// unless it is set, and that seed is the whole state of the RNG a passive open takes its initial sequence
    /// number from - so an unseeded interface makes every session of this daemon issue the *same* sequence of
    /// ISNs. Restart the daemon and the first client to be accepted gets the number the last session's first
    /// client got, which is exactly the case smoltcp's own documentation warns about. Nothing of the previous
    /// session survives to notice: its sockets went with the process, so the only thing that can arrive from
    /// the old connection is a segment the *network* delayed, matched by tuple and sequence alone.
    ///
    /// Deterministic rather than statistical. Two *fixed* seeds are driven through the real accept path and
    /// the two SYN-ACKs are compared, so this asserts that the configured seed is what selects the sequence
    /// space, without ever asserting that a random number looks random. The production entropy read is proved
    /// separately, where it lives - see [crate::tun_reader].
    #[tokio::test]
    async fn a_session_seed_reaches_the_stacks_initial_sequence_numbers() {
        // The initial sequence number a stack seeded with `seed` chooses for its first passive open, read
        // off the SYN-ACK it really emitted. A SYN consumes one sequence number, so what the client would
        // acknowledge is that number plus one - which makes it exactly as good a fingerprint.
        async fn first_isn(seed: u64) -> TcpSeqNumber {
            let mut admission = admission_for(2);
            let gate = Arc::new(Gate::default());
            let mut session = seeded_session(&mut admission, &gate, seed).await;
            let client = client(12_000);
            let packet = segment(
                client,
                DESTINATION,
                TcpRepr {
                    src_port: client.port(),
                    dst_port: DESTINATION.port(),
                    control: TcpControl::Syn,
                    seq_number: TcpSeqNumber(1_000),
                    ack_number: None,
                    window_len: 32_768,
                    window_scale: None,
                    max_seg_size: Some(1_400),
                    sack_permitted: false,
                    sack_ranges: [None; 3],
                    timestamp: None,
                    payload: &[],
                },
            );
            let peeked = vpnhotspotd::shared::tcp_wire::peek(&packet).expect("a TCP segment");
            session.engine.accept(
                &packet,
                peeked,
                false,
                Instant::now(),
                &mut session.output,
                &mut admission,
            );
            let (_, written) = session
                .queue
                .dequeue()
                .expect("the stack answered the client's SYN");
            let wire = parse(&written);
            assert_eq!(wire.control, TcpControl::Syn, "a SYN-ACK");
            let isn = wire.acknowledging;
            session
                .engine
                .shutdown(&mut admission, &mut session.output)
                .await;
            session.release(&mut admission);
            assert_eq!(admission.invariant_violations(), 0);
            isn
        }

        let first = first_isn(1).await;
        let second = first_isn(2).await;
        assert_ne!(
            first, second,
            "two seeds have to put the two sessions in different sequence spaces"
        );
        // And neither is what an unseeded interface would produce, which is the state this replaced.
        let default = first_isn(0).await;
        assert_ne!(first, default);
        assert_ne!(second, default);
    }

    /// An engine that can afford no flows at all still builds tables, and the charge it took covers them.
    ///
    /// The fail-open case this closes: a derived bound of zero read as "no allocation", while the minimum a
    /// bounded channel cannot avoid was built anyway and nobody had charged for it. Measured against what the
    /// engine really holds rather than argued.
    #[tokio::test]
    async fn a_zero_flow_engine_charges_for_the_tables_it_builds() {
        let mut admission = admission_for(0);
        let baseline = admission.bytes_charged();
        let (engine, _markers, _asks) = Engine::new(MTU, SEED, &mut admission).expect("an engine");
        assert_eq!(engine.prepared, 0, "nothing it could afford a flow for");

        // The minimum both channels really allocate, from the channels themselves.
        assert_eq!(engine.ready.max_capacity(), flow_setup::ready_depth(0));
        assert_eq!(engine.ready.max_capacity(), 1, "and one is the minimum");
        assert_eq!(engine.ready.capacity(), 1, "with nothing in it");
        assert_eq!(
            engine.asks.max_capacity(),
            submission_depth(admission.dns_token_cap())
        );
        assert_eq!(engine.flows.prepared(), 0);
        assert_eq!(
            engine.fair.prepared(),
            0,
            "and its fair queue is prepared for none either"
        );
        assert_eq!(engine.sockets.iter().count(), 0);

        // What it was charged, and what that has to cover.
        let charged = admission
            .granted(&engine.tables)
            .expect("the tables are held")
            .bytes;
        let expected =
            tables_footprint(0, MTU, admission.dns_token_cap()).expect("a bounded footprint");
        assert_eq!(charged, expected, "charged for what it built");
        let channels = channel_footprint::<Event>(engine.ready.max_capacity(), engine.prepared)
            .expect("a bounded footprint")
            + channel_footprint::<tcp_dns::Ask>(engine.asks.max_capacity(), engine.prepared)
                .expect("a bounded footprint");
        assert!(
            charged >= channels,
            "the two minimum channels alone are {channels} and the charge is {charged}"
        );
        assert!(
            charged >= MTU as u64,
            "and the device's one output slot is in it too"
        );

        // A flow is refused, and not because a table is full - the aggregate is what has no room.
        let gate = Arc::new(Gate::default());
        let (writer, queue, _terminals) = tun_writer::channel();
        let mut session = Session {
            engine,
            output: Output::testing(MTU, 8, writer),
            markers: _markers,
            asks: _asks,
            queue,
        };
        session.engine.upstreams_gated_by(gate.clone());
        session
            .engine
            .apply(
                Stamp::default(),
                Some(1),
                MTU,
                &mut admission,
                &mut session.output,
            )
            .await;
        assert!(!session.engine.open(
            client(10_004),
            DESTINATION,
            64,
            false,
            Instant::now(),
            &mut admission
        ));
        assert_eq!(session.engine.counters.unprepared, 1);
        assert_eq!(session.engine.sockets.iter().count(), 0);
        assert_eq!(gate.entered.load(Relaxed), 0);
        session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A config that names a network but no interface still opens TCP work, because a terminated connection
    /// has no unconnected reply to check an interface against.
    ///
    /// The shape is real: the app selects an upstream before it can resolve that upstream's interface index,
    /// and the two arrive in separate observations. Refusing TCP and the resolver for the whole of that
    /// window - which is what one combined `Option<Upstream>` did - is a session that cannot resolve a name
    /// until an unrelated lookup happens to succeed.
    #[tokio::test]
    async fn a_selected_network_without_an_interface_still_carries_tcp() {
        use vpnhotspotd::shared::egress::{self, Egress};
        use vpnhotspotd::shared::proto::daemon::ShizukuSessionConfig;

        let egress = egress::decode(&ShizukuSessionConfig {
            sequence: 1,
            upstream_generation: 1,
            downstream_epoch: 1,
            admit: true,
            upstream_network: Some(0x1234),
            upstream_interface_index: None,
            virtual_addresses: Vec::new(),
            gateway_addresses: Vec::new(),
            downstream_mtu_floor: 0,
        })
        .expect("a network with no interface is a shape a session goes through");
        assert_eq!(
            egress,
            Egress {
                selected_network: Some(0x1234),
                relay_upstream: None,
            },
            "the relays get nothing, and the engine gets the handle"
        );

        let prepared = 4usize;
        let mut admission = admission_for(prepared);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let (engine, markers, asks) = Engine::new(MTU, SEED, &mut admission).expect("an engine");
        let (writer, queue, _terminals) = tun_writer::channel();
        let mut session = Session {
            engine,
            output: Output::testing(MTU, 8, writer),
            markers,
            asks,
            queue,
        };
        session.engine.upstreams_gated_by(gate.clone());
        // Exactly what the ingress owner hands the engine from this config, both axes included - so the
        // successor below advances the generation alone, which is the case the retirement split is about.
        session
            .engine
            .apply(
                Stamp {
                    generation: 1,
                    epoch: 1,
                },
                egress.selected_network,
                MTU,
                &mut admission,
                &mut session.output,
            )
            .await;

        assert!(
            session.engine.open(
                client(10_500),
                DESTINATION,
                64,
                false,
                Instant::now(),
                &mut admission
            ),
            "the flow opens on the selected network alone"
        );
        tokio::task::yield_now().await;
        assert_eq!(gate.entered.load(Relaxed), 1, "and its worker really ran");
        assert_eq!(
            gate.networks.lock().expect("no worker panicked").as_slice(),
            [0x1234],
            "the worker connects on exactly the selected network"
        );
        assert_eq!(session.engine.counters.no_upstream, 0);

        // And the resolver submission owner asks on that exact handle - observed where the platform call is
        // made, not where the config was decoded.
        let (answers, mut asked) = tokio::sync::mpsc::unbounded_channel();
        session.engine.queries.answered_by(answers);
        assert!(session.engine.open(
            client(10_501),
            RESOLVER,
            64,
            true,
            Instant::now(),
            &mut admission
        ));
        let handle = *session
            .engine
            .flows
            .keys()
            .max()
            .expect("the DNS-over-TCP flow");
        let downstream = session
            .engine
            .flows
            .get(&handle)
            .expect("held")
            .record
            .downstream
            .clone()
            .expect("open");
        downstream
            .send(Owned::new(dns_wire::frame(&question(9)).expect("framed")))
            .await
            .expect("the transport is reading");
        // Two steps: the length the client announced, admitted before anything is stored, and then the exact
        // query, accepted and published on the selection current at that moment.
        for _ in 0..2 {
            let ask = session.asks.recv().await.expect("the transport asked");
            session.engine.ask(ask, true, &mut admission);
        }
        let submitted = asked.recv().await.expect("a transaction asked");
        assert_eq!(
            submitted.network, 0x1234,
            "the resolver was asked on the selected network"
        );

        // A handover, through the same path: the generation advances, every flow that holds a
        // selected-network socket is retired, and the next worker opens against the handle the *new* config
        // named. The record is what proves it - a stale selection reaching a new worker would look identical
        // in every count. The DNS-over-TCP transport beside it is not retired at all, which is what
        // [Retirement::Upstreams] means and what the tests below are about.
        let successor = egress::decode(&ShizukuSessionConfig {
            sequence: 2,
            upstream_generation: 2,
            upstream_network: Some(0x5678),
            upstream_interface_index: None,
            virtual_addresses: Vec::new(),
            gateway_addresses: Vec::new(),
            downstream_epoch: 1,
            admit: true,
            downstream_mtu_floor: 0,
        })
        .expect("a handover to a network whose interface is not yet resolved");
        session
            .engine
            .apply(
                Stamp {
                    generation: 2,
                    epoch: 1,
                },
                successor.selected_network,
                MTU,
                &mut admission,
                &mut session.output,
            )
            .await;
        assert_eq!(
            gate.left.load(Relaxed),
            1,
            "the old generation's worker was cancelled by the handover"
        );
        assert_eq!(
            session.engine.flows.len(),
            1,
            "and the DNS-over-TCP transport, which holds no such socket, was left alone"
        );
        assert!(session.engine.open(
            client(10_502),
            DESTINATION,
            64,
            false,
            Instant::now(),
            &mut admission
        ));
        tokio::task::yield_now().await;
        assert_eq!(
            gate.networks.lock().expect("no worker panicked").as_slice(),
            [0x1234, 0x5678],
            "the retired handle never reached the successor's worker"
        );

        session
            .engine
            .shutdown(&mut admission, &mut session.output)
            .await;
        session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
    }
}
