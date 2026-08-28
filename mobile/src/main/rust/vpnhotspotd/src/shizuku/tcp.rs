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
//! Those bytes are held rather than sent or dropped, and nothing of this owner's holds them: a half that
//! cannot yet send is one the crossing does not read the bridge for, so they stay in the bridge exactly where
//! the upstream half left them, and the client's own final ACK is what runs the crossing again. See
//! [vpnhotspotd::shared::bridge].

use std::net::SocketAddr;
use std::time::Instant;

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketStorage};
use smoltcp::socket::tcp::Socket;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::icmp_nat::{nat66_hop_limit, Nat66HopLimit};
use vpnhotspotd::shared::model::Network;
use vpnhotspotd::shared::workers::{Ended, Identity, Workers};

use crate::shizuku::flow_setup;
use vpnhotspotd::shared::admission::{
    largest_fitting, linear_footprint, Admission, Class, Denied, Lease, Request,
};
use vpnhotspotd::shared::bridge::{Bridge, Worker};
use vpnhotspotd::shared::dns_debt::Connection;
use vpnhotspotd::shared::flow::{self, FlowId, Turns};
use vpnhotspotd::shared::flow_budget;
use vpnhotspotd::shared::ingress as boundary;
use vpnhotspotd::shared::reply_bound::{built_depth, channel_footprint};

use crate::report;
use crate::shizuku::output::Output;
use crate::shizuku::owned::Owned;
use crate::shizuku::tcp_device::Shim;
use crate::shizuku::tcp_dns::{self, Serving, Transactions};
use crate::shizuku::tcp_flow;
use crate::shizuku::tun_writer::Stamp;

mod bridge;
mod dns;
mod ingress;
mod lifetime;
mod terminal;

// The three endings and the one place a flow is given back, which is a decision of its own - see
// [crate::shizuku::tcp::terminal] - and the one traffic answer that carries them to the owning task, which is
// [crate::shizuku::tcp::bridge]'s.
pub(crate) use bridge::Attention;

/// What one entry in smoltcp's socket set really costs the engine: the storage slot the set holds inline,
/// beyond the two buffers the flow itself is charged for.
///
/// `SocketStorage` rather than a handle-sized proxy, and the difference is not small: the slot holds an
/// `Option<Item>` containing the socket enum itself, so a handle-sized figure understated it by roughly the
/// size of a TCP control block per prepared flow. Taken from the type, so it cannot drift from what smoltcp
/// actually allocates.
const SOCKET_SLOT_BYTES: u64 = std::mem::size_of::<SocketStorage<'static>>() as u64;

/// What one flow costs the aggregate, at the types this engine really holds.
///
/// The equation is [vpnhotspotd::shared::flow_budget]'s rather than this module's, and so is the sizing it
/// reads: one value is what the solver below charges against and what [flow_setup::prepare] builds the bridge
/// and the channels at, so neither can be one size in the reservation and another in the construction.
/// `None` is a figure that would wrap, which is a flow that cannot be accounted for and therefore must not be
/// built.
fn flow_footprint() -> Option<u64> {
    flow_budget::footprint::<Owned, tcp_dns::Control>(&flow_budget::SIZING)
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
    /// Every flow, which is the session ending: it owes nobody anything, and every descriptor it opened has
    /// to be back before the process is.
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

/// What the client-side stack needs from a config, and what a change to it does to this flow.
struct Flow {
    /// The TUN-visible endpoints, which with the family are the key the design specifies. The generation is not
    /// in it because neither kind of flow needs it there. An upstream flow is retired when the generation
    /// changes, so two of them can never contend for this key; a resolver transport is generation-independent
    /// and deliberately survives the change, and what carries a generation on its behalf is each query the
    /// owner stamps as it accepts it and each packet the owner emits for it - never the flow.
    client: SocketAddr,
    destination: SocketAddr,
    /// Which worker this flow's signals must name. Kept beside the handle because smoltcp reuses handles: a
    /// terminal or a DNS-over-TCP request naming only a handle cannot be told apart from one belonging to
    /// the flow that reused it, and acting on the difference is a reset sent to the wrong client.
    worker: u64,
    /// What this flow carries, which is what a config change consults - never the transaction below.
    kind: Kind,
    /// Everything this flow owns: its record and upstream descriptor, its two stack buffers and both
    /// directions of its byte bridge - and, for a DNS-over-TCP flow, the one logical resolver token its
    /// transport holds for its whole life.
    ///
    /// Not an exchange's worth of bytes: those belong to the debt each submitted query takes, which is what
    /// lets them stay charged when this transport closes over a question still in flight. See
    /// [vpnhotspotd::shared::dns_debt].
    connection: Connection,
    /// This owner's half of the flow's byte bridge: an ordinary bounded Tokio stream, read straight into the
    /// client's send buffer and written straight out of its receive buffer. Both directions' backpressure is
    /// the buffer filling and both directions' wake is the library's, so nothing travels between this owner
    /// and the flow's task to say work is waiting - see [vpnhotspotd::shared::bridge].
    bridge: Bridge,
    /// Set by a crossing toward the client, and cleared by the lifetime refresh that reads it. Kept per flow
    /// rather than answered per pass, because one pass can deliver to every flow and each of them owns its
    /// own idle floor - see [Engine::traffic].
    refresh: bool,
    /// This owner's half of the DNS control pair, the reservation this transport currently holds, the
    /// transaction it opened and the delivery parked for its answer - all of it built with the flow and
    /// charged with it. Present on every flow, because both kinds are built by one [flow_setup::prepare];
    /// an ordinary spliced flow simply never asks anything of it. See [crate::shizuku::tcp_dns::Serving].
    serving: Serving,
    /// When this flow falls idle, from the phase its socket was last observed in. `None` only for
    /// `TIME-WAIT`, whose cleanup is smoltcp's own protocol timer rather than this owner's - see
    /// [crate::shizuku::tcp::lifetime].
    deadline: Option<Instant>,
    /// Set once this flow's worker has run to completion cleanly and the client side has not finished yet.
    ///
    /// The state that keeps a terminating close honest. A worker returns as soon as *its* ordered work is
    /// done, and for either kind that means as soon as its last bytes and its ordered end of stream are **in
    /// the bridge**, not delivered. At that moment the client socket is typically still `ESTABLISHED` with
    /// bytes to deliver, or in `LAST-ACK`, `CLOSING` or `TIME-WAIT` with a FIN to retransmit and a final
    /// acknowledgment to wait for. Removing the flow there took the client's half of the connection away
    /// mid-teardown, and now takes undelivered bytes with it.
    ///
    /// So a clean terminal *detaches* instead: the worker's descriptor is gone with its task, and what is
    /// left is a client-side-only flow that still owns its socket, its conservative grant, its DNS state and
    /// whatever its half of the bridge still holds - until smoltcp reaches `Closed`, its outer floor runs
    /// out, a config retires it, or the session ends. What the worker left in the bridge stays readable, and
    /// the end of the stream it shut its write half down for before returning follows it - a `simplex` half
    /// signals nothing on drop - so the engine goes on crossing exactly as it did while the worker existed;
    /// what the bridge cannot get any more is anything new. No task of its own and
    /// no *per-flow* timer task stands behind it - its teardown is still scheduled, by the engine's combined
    /// stack-and-floor deadline, which is what lets the remaining bytes be written and the FIN
    /// retransmitted. The ingress owner polls for it, exactly as it polls for a settled resolver
    /// transaction. See [Engine::detached] and [Engine::settled].
    detached: bool,
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
    /// A token that could not be handed to the question it belonged to.
    unsettled: u64,
    /// Workers that finished cleanly while their client's teardown was still running, so the flow outlived
    /// its worker instead of being removed under a half-finished close.
    detached: u64,
    no_upstream: u64,
    /// Everything the packet boundary counts, in the shape that boundary defines - see
    /// [boundary::Counters]. One home per figure, and the selection of which figure a
    /// given outcome moves is that module's rather than this one's.
    ingress: boundary::Counters,
    /// Flows this owner took back because they had been idle past their phase's floor. Counted apart from
    /// [Counters::closed] because an expiry is a client-visible reset the client did not ask for, and apart
    /// from [Counters::reset] because a listening flow expires without one.
    expired: u64,
    closed: u64,
    to_client: u64,
}

impl Counters {
    fn describe(&self) -> String {
        format!(
            "opened {} resolved {} answered-here {} denied {} preserved {} no-upstream {} reset {} \
             tail-failed {} expired {} detached {} closed {} to-upstream {} to-client {} stale {} \
             unconsumed {}",
            self.opened,
            self.resolved,
            self.answered_here,
            self.denied,
            self.preserved,
            self.no_upstream,
            self.ingress.reset,
            self.ingress.tail_failed,
            self.expired,
            self.detached,
            self.closed,
            self.ingress.to_upstream,
            self.to_client,
            self.ingress.stale,
            self.ingress.unconsumed
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
    /// because a retirement joins that one and must not join this one - see [crate::shizuku::tcp_dns].
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
    /// Whose turn it is, in both directions. Explicit because iterating a `HashMap` is an arbitrary order
    /// that changes when the map is resized, which is not fairness but a different unfairness each time - and
    /// because it is what indexes the live flows for every walk this engine makes over them.
    outgoing: Turns<SocketHandle>,
    /// The flow table, the round-robin order, the socket set, the ask channel and the device's output slot:
    /// byte-only owners prepared at session start and charged once. A flow's own bridge is not here - it is
    /// charged per flow, with the flow that owns it.
    tables: Lease,
    /// How many live flows every table above was prepared for. One number, so no two of them can disagree
    /// about what the engine may hold.
    prepared: usize,
    counters: Counters,
    /// The base for smoltcp's monotonic millisecond clock, which has to be the same instant for the whole
    /// session or its timers jump.
    started: Instant,
}

impl Engine {
    /// `seed` is this session's own, read from the kernel by [crate::shizuku::tun_reader::prepare]. It is not
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
    ) -> Result<(Self, mpsc::Receiver<tcp_dns::Ask>), Denied> {
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
        // Charged before the channel exists, and charged for the depth that will exist: a queue built at a
        // minimum depth because the derived bound was zero is still a real allocation, and one nobody charged
        // for is the fail-open case the aggregate exists to prevent. [tables_footprint] is what both the
        // solver above and this reservation read, so the two cannot disagree about any of it.
        let bytes =
            tables_footprint(prepared, mtu, admission.dns_token_cap()).ok_or(Denied::Arithmetic)?;
        let tables = admission.reserve(Request::bytes(bytes, Class::General))?;
        // One outstanding ask per logical token, which is exactly how many transports can be asking at
        // once: a transport cannot exist without a token, and it asks one thing at a time - a length to
        // admit, the query that length was admitted for, or the delivery that answered it.
        //
        // The engine's one channel, and there is no second: payload does not travel to this owner and
        // neither does a wake for it, because every flow's own bridge is what wakes this owner - see
        // [crate::shizuku::tcp::bridge].
        let (asks, asking) = mpsc::channel(submission_depth(admission.dns_token_cap()));
        let queries = match Transactions::new(admission) {
            Ok(queries) => queries,
            Err(why) => {
                // Both halves of the channel go before the grant that covers them. Left to the unwind they
                // would drop *after* this release, which is the same fail-open moment as releasing while the
                // ingress task still held a receiver - see [Engine::release].
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
                outgoing: Turns::with_capacity(prepared),
                tables,
                prepared,
                counters: Counters::default(),
                started,
            },
            asking,
        ))
    }

    /// Releases the engine's own capacity, after every flow and transaction has been settled.
    /// Gives this engine's own retained capacity back, once everything it covers is physically gone.
    ///
    /// The receiver comes in by value rather than outliving the call, for the same reason the UDP and Echo
    /// relays take theirs: the `tables` lease below covers the ask channel *whole* - shared state, blocks and
    /// the messages in its slots - so releasing while the ingress task still owned the receiving end would be
    /// capacity given back for allocations this process was still holding. Taking it by value is what makes
    /// that order structural instead of a comment; there is no drain, because dropping a receiver destroys
    /// whatever it had buffered and every sender is already gone by here.
    pub(crate) fn release(self, asking: mpsc::Receiver<tcp_dns::Ask>, admission: &mut Admission) {
        // The transaction table's own lease first - it is a separate grant with its own contents.
        self.queries.release(admission);
        // Then everything `tables` pays for, before the grant goes - and it is worth reading this against
        // [tables_footprint] item by item, because that is the list this has to match: the worker table, the
        // round-robin order, both halves of the ask channel, the socket set, and the device's one MTU-sized
        // output slot. The device was the one that used to fall out of scope *after* the release, which is
        // the same fail-open moment as a receiver outliving the grant that covers it.
        drop(self.flows);
        drop(self.outgoing);
        drop(self.sockets);
        drop(self.asks);
        drop(asking);
        drop(self.device);
        admission.release(self.tables);
    }

    fn now(&self) -> SmolInstant {
        SmolInstant::from_micros(self.started.elapsed().as_micros() as i64)
    }

    /// Adopts a config, retiring what the generation invalidates rather than everything.
    ///
    /// The distinction is what stops a DNS-over-TCP client's connection being collateral damage of every
    /// handover: the generation retires exactly the flows that hold a socket bound to the network that
    /// changed, and a virtual-DNS transport holds none. It terminates locally, its answers come from the
    /// platform resolver, and which network each of its queries went out on is fixed one query at a time when
    /// this owner accepts it - so the transport itself is untouched, keeps its socket, its bridge and its one
    /// logical token, and the client goes on using the connection it opened.
    ///
    /// Nothing here cancels or awaits a resolver transaction: cancelling would return this process's
    /// descriptor and nothing of the platform's work, and awaiting one would make the config acknowledgement
    /// wait on a remote name server. A transaction that outlives its config settles into a SERVFAIL for its
    /// own query - see [Engine::settle].
    ///
    /// Returns once every task this retirement did cancel has been joined and its descriptor closed.
    pub(crate) async fn apply(
        &mut self,
        stamp: Stamp,
        upstream: Option<Network>,
        admission: &mut Admission,
        output: &mut Output,
    ) {
        let retiring = (stamp != self.stamp).then_some(Retirement::Upstreams);
        // Adopted before the sweep, because whatever reset a swept flow owes its client is written during it:
        // the
        // writer gates a dequeued packet on the current retirement, so a reset stamped with the one being swept
        // would be purged along with the traffic it is meant to terminate. It is also what a query accepted
        // after this point is stamped with, which is the successor by construction.
        self.stamp = stamp;
        self.upstream = upstream;
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
            // holds each live handle exactly once - see [vpnhotspotd::shared::flow::admit_flow] - and a list
            // built here would be scratch sized by traffic that no lease covers. Destructured because the
            // walk reads one field while the steps below write three others.
            let Engine {
                flows,
                sockets,
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
                // Cancelling is the whole of it, and the whole of it is abortive: whatever either direction
                // of the bridge still holds is discarded with the bridge when the flow is reclaimed, and
                // every wait a worker can be in - a read, a write, a bridge with no room - races this token.
                // There is nothing to hand back first, because nothing of this flow's is anywhere but in the
                // bridge this owner still holds.
                held.cancel.cancel();
                // At most one terminal packet per retired flow, written before anything is freed, so a client
                // fails fast instead of waiting out its own retransmissions against a connection nothing will
                // answer again. Built here rather than at close, because close removes the socket that
                // carries it.
                let socket = sockets.get_mut::<Socket>(*handle);
                // Only a socket with a remote endpoint can be told: an eligible connected state builds and
                // counts one reset, while one still listening or already closed is aborted in silence -
                // there is nowhere for the stack to send it, and counting it would overstate what was sent.
                if socket.remote_endpoint().is_some() {
                    counters.ingress.reset += 1;
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
        // Nothing of a retired flow's is drained here, because there is nowhere left to drain to: its bytes
        // live in the bridge it owns, which dies with the record in [Engine::reclaim], and a task parked on
        // that bridge wakes on its own token rather than on anything this loop does.
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
    /// idle lifetime is measured from - see [crate::shizuku::tcp::lifetime].
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
    ) -> Option<SocketHandle> {
        let source = if resolver {
            Source::Resolver
        } else {
            let Some(upstream) = self.upstream else {
                self.counters.no_upstream += 1;
                return None;
            };
            Source::Upstream(upstream)
        };
        // The remaining hop limit is validated before the connect, and an expired one is not connected at all:
        // a terminated flow cannot preserve it, but it can refuse to launder a packet that should have died.
        let Nat66HopLimit::Forward(hop_limit) = nat66_hop_limit(Some(hop_limit)) else {
            return None;
        };
        // Destructured so the transaction below can borrow the pieces it needs disjointly: the round-robin
        // order is what it registers into, and everything else is what its operations build.
        let Engine {
            sockets,
            flows,
            outgoing,
            prepared,
            asks,
            sweep,
            counters,
            ..
        } = self;
        let mut ops = Admit {
            sockets,
            flows,
            asks,
            sweep,
            counters,
            admission,
            client,
            destination,
            hop_limit,
            source,
            now,
        };
        // One transaction, and production's own: capacity in both tables before a descriptor is opened or a
        // byte charged, then build, then register the round-robin order, then admit - with every failure
        // after the build unwinding the socket, the bridge and the grant. See
        // [vpnhotspotd::shared::flow::admit_flow].
        match flow::admit_flow(&mut ops, outgoing, *prepared) {
            Ok(handle) => {
                self.counters.opened += 1;
                if resolver {
                    self.counters.resolved += 1;
                }
                // The handle, because the caller has to be able to take this flow back: a segment the stack
                // then refuses leaves a socket, a grant and a worker that nothing will ever use.
                Some(handle)
            }
            Err(flow::Refused::AtCapacity(_)) => {
                self.counters.unprepared += 1;
                None
            }
            // Already counted and, where it deserved one, reported by the operation that failed.
            Err(flow::Refused::Unbuildable(())) => None,
        }
    }

    /// Runs the stack until it has nothing more to do. Called after anything that could have changed a
    /// socket, because smoltcp is a state machine that only advances when polled.
    ///
    /// It moves no bytes, deliberately. Every crossing between a flow's bridge and its socket happens inside
    /// the poll the ingress task makes, so that the wake a blocked direction registers is that task's own -
    /// see [crate::shizuku::tcp::bridge]. The owner re-enters that crossing immediately after this returns,
    /// which is what makes an acknowledgement or an arriving segment the wake for the two directions no
    /// bridge can register for.
    /// There is deliberately no variant of this that takes an instant. The packet boundary needs one - it
    /// polls twice and has to attribute a socket's transition to the packet in between, which two polls at
    /// the *same* instant make exact - but it also needs the scan below to run once, at the end, rather than
    /// inside each poll. Handing it a "poll at this instant" that did both is what put the scan first; the
    /// two primitives it composes are [Engine::quiesce] and [Engine::reclaim_closed], and this is the
    /// non-packet callers' composition of them.
    pub(crate) fn poll(&mut self, output: &mut Output) {
        self.quiesce(self.now(), output);
        self.reclaim_closed();
    }

    /// **Only** the stack, the device and the output, at the instant given. No flow of this engine's is
    /// examined and no worker is told anything.
    ///
    /// That restriction is the whole reason this is its own operation. It is the primitive
    /// [vpnhotspotd::shared::ingress] settles with, and that sequence must be the only thing deciding what
    /// happens to a flow while it runs: an accepted reset has to fence its socket *before* its worker is
    /// cancelled, and a client's FIN has to arm its idle floor before its ending is extracted. A poll that
    /// cancelled workers on its way past would do both of those first and out of order - which is exactly
    /// what it did while [Engine::reclaim_closed] lived here.
    fn quiesce(&mut self, at: SmolInstant, output: &mut Output) {
        loop {
            let progressed = self.interface.poll(at, &mut self.device, &mut self.sockets);
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
            if matches!(progressed, PollResult::None) && !emitted {
                break;
            }
        }
    }

    /// The Closed-socket scan: what a socket the stack has finished with does to the worker still attached
    /// to it.
    ///
    /// Run *after* whatever changed those sockets, never inside it. Every non-packet caller reaches it
    /// through [Engine::poll]; the packet boundary reaches it as a primitive of its own, which
    /// [vpnhotspotd::shared::ingress::accept] runs last, when it has finished deciding - once for every
    /// packet the stack saw, and not at all for a `SYN` whose admission was refused before anything was
    /// built, because that returns having changed nothing to scan.
    ///
    /// What a `Closed` socket means for its worker is [Bridge::teardown]'s decision and not this walk's -
    /// including the one exception, a client that half-closed cleanly and left its worker still flushing
    /// bytes this daemon acknowledged.
    ///
    /// Walked over the round-robin order rather than into a list of handles: that order already holds each
    /// live handle exactly once, so a list would be scratch proportional to live flows that no lease covers,
    /// allocated on the busiest path in the engine. The borrows below are of separate fields, which is what
    /// lets the walk read one while the step reads the other.
    pub(super) fn reclaim_closed(&mut self) {
        let Engine {
            flows,
            sockets,
            outgoing,
            ..
        } = self;
        debug_assert_eq!(
            outgoing.len(),
            flows.len(),
            "the round-robin order indexes exactly the live flows"
        );
        for handle in outgoing.iter() {
            let Some(held) = flows.get(handle) else {
                continue;
            };
            // An attached flow's worker is told to stop and the release follows joining it, so the descriptor
            // is gone before the accounting says so; a detached flow has no worker left, and the ingress
            // owner's own scan is what settles it - see [Engine::detached].
            held.record
                .bridge
                .teardown(sockets.get::<Socket>(*handle).state(), &held.cancel);
        }
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
/// The engine's own flow-admission operations, as [flow::admit_flow] drives them.
///
/// A borrowed view rather than the whole engine, so the transaction can hold the round-robin order at the
/// same time as the tables these touch.
struct Admit<'a> {
    sockets: &'a mut flow_setup::Sockets,
    flows: &'a mut Workers<SocketHandle, Flow>,
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
}

/// What one built-but-not-yet-admitted flow holds: the record the worker table will take, and the pieces the
/// worker itself needs. Kept together so an unwind drops all of them.
struct Built {
    flow: Flow,
    identity: Identity,
    /// Taken by the worker future when one is started. Absent afterwards, so an unwind on the admission path
    /// drops what is left rather than pretending to hold what the future already owns.
    stream: Option<Worker>,
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
            stream: None,
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
            flow_setup::Sizing {
                bytes,
                // The same value the charge above was computed from, so the channels below are built at the
                // depths that were paid for.
                flow: flow_budget::SIZING,
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
            bridge,
            stream,
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
                    bridge,
                    refresh: false,
                    serving,
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
                },
                identity,
                stream: Some(stream),
                control: Some(control),
                filled: Some(filled),
            },
        )
    }
}

impl flow::FlowOps for Admit<'_> {
    type Handle = SocketHandle;
    type Record = Built;
    /// Already counted and reported where it deserved one, so there is nothing left to say about it.
    type Error = ();

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
        Ok((handle, worker, built))
    }

    fn unwind(&mut self, handle: SocketHandle, _worker: u64, record: Built) {
        let Built {
            flow,
            stream,
            control,
            filled,
            ..
        } = record;
        let Flow {
            connection,
            bridge,
            serving,
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
                bridge,
                stream,
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
    // holding by then is a socket, a lease, both halves of a byte bridge and two control channels. Boxing it
    // would put an allocation on the one path that runs because the daemon is already out of room.
    #[allow(clippy::result_large_err)]
    fn start(&mut self, handle: SocketHandle, record: Built) -> Result<(), Built> {
        let Built {
            flow,
            identity,
            stream,
            control,
            filled,
        } = record;
        let (Some(stream), Some(control), Some(filled)) = (stream, control, filled) else {
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
                    tcp_dns::serve(
                        FlowId::new(handle, identity.id),
                        stream,
                        asks,
                        control,
                        filled,
                        token,
                    ),
                )
                // The worker future took the bridge's worker half and both control halves, so what comes
                // back cannot carry them - and does not need to: they are dropped with the future that was
                // never spawned.
                .map_err(|(flow, _)| Built::without_worker(flow, identity));
        };
        // An ordinary flow's transport never reads these, and dropping them here is what says so: the
        // channels themselves are charged with the flow either way, because one preparation builds both
        // kinds.
        drop(control);
        drop(filled);
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
                    connected = crate::shizuku::egress::connect_tcp(upstream, destination) => connected,
                };
                match connected {
                    Ok(socket) => match tokio::net::TcpStream::from_std(socket.into()) {
                        Ok(upstream) => tcp_flow::splice(upstream, stream, token, sweep).await,
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
        .checked_add(linear_footprint(
            flows,
            std::mem::size_of::<SocketHandle>() as u64,
        )?)?
        // The channel a transport asks its owner on, at the type it really carries: every variant of
        // [tcp_dns::Ask], not the one shape a query used to travel in. Fan-in: every DNS transport clones
        // the ask sender, so it carries one producer per prepared flow. It is the engine's only channel -
        // there is no readiness channel and no payload channel, because a flow's bytes and their wakes both
        // live in the bridge that flow owns.
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

fn endpoint(address: SocketAddr) -> IpEndpoint {
    IpEndpoint::new(address.ip().into(), address.port())
}
