//! Every resource one intercepted TCP flow needs before it may exist, and the exact reverse of acquiring
//! them.
//!
//! A lease, a socket with two buffers in a real set, five bounded channels and a worker identity are taken in
//! an order where each step can undo the ones before it. The engine keeps the record it stores, the future it
//! spawns and the reports it writes, and calls these two functions for the rest.
//!
//! # Why preparation and release are one pair
//!
//! A registration that fails half-way is the only interesting case, and it is interesting because the failure
//! can arrive from three directions: the grant, the client-side stack, or the identity table. Each unwinds
//! what the ones before it took, and [release] is the same reversal written once for the failures that arrive
//! *after* preparation has finished - a worker table that refuses, a fair queue that will not register. Two
//! copies of that reversal is how a socket outlives its lease.

use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::tcp::{ListenError, Socket, SocketBuffer};
use smoltcp::wire::IpListenEndpoint;
use tokio::sync::mpsc;

use crate::report;
use crate::shizuku::owned::Owned;
use crate::shizuku::tcp_dns::{Control, Serving};
use crate::shizuku::workers::{Identity, Workers};
use vpnhotspotd::shared::admission::Admission;
use vpnhotspotd::shared::dns_debt::{self, Connection};
use vpnhotspotd::shared::fair::FlowId;
use vpnhotspotd::shared::flow_budget;
use vpnhotspotd::shared::mailbox::Mailbox;
use vpnhotspotd::shared::reply_bound::built_depth;
use vpnhotspotd::shared::room::Room;
use vpnhotspotd::shared::transfer::Transfer;

/// The client-side stack's socket storage, and how much of it has ever really been allocated.
///
/// smoltcp's set is backed by a `Vec` it pushes to when no slot is free, and it exposes neither that vector's
/// length nor its capacity - so "the set never grew past what was charged for" is not a question the set can
/// be asked. It is asked here instead, at the one boundary where the vector can grow: a new slot is only
/// pushed when every existing one is occupied, so the high-water mark of occupancy *is* the backing length.
/// Nothing is inferred from handles, which name slots and say nothing about how many exist.
pub(crate) struct Sockets {
    set: SocketSet<'static>,
}

impl Sockets {
    pub(crate) fn new(prepared: usize) -> Self {
        Self {
            set: SocketSet::new(Vec::with_capacity(prepared)),
        }
    }

    /// Adds one socket, pushing a slot only when none is free - which is exactly when the backing vector
    /// grows.
    fn add(&mut self, socket: Socket<'static>) -> SocketHandle {
        self.set.add(socket)
    }

    pub(crate) fn remove(&mut self, handle: SocketHandle) -> smoltcp::socket::Socket<'static> {
        self.set.remove(handle)
    }
}

impl std::ops::Deref for Sockets {
    type Target = SocketSet<'static>;

    fn deref(&self) -> &SocketSet<'static> {
        &self.set
    }
}

impl std::ops::DerefMut for Sockets {
    fn deref_mut(&mut self) -> &mut SocketSet<'static> {
        &mut self.set
    }
}

/// How large the pieces of one flow are, and what its composite grant covers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sizing {
    /// What the composite grant covers: both stack buffers, every payload chunk that can exist at once, and
    /// every one of the per-flow channels built below. Computed from `flow` by
    /// [vpnhotspotd::shared::flow_budget::footprint], which is also what the engine's own solver reads - so
    /// the charge and the construction below cannot disagree about a depth.
    pub(crate) bytes: u64,
    /// The one value both the charge above and the channels below are sized from.
    pub(crate) flow: flow_budget::Sizing,
    /// The hop limit replies inherit, decided by the NAT66 rules rather than here.
    pub(crate) hop_limit: u8,
    /// Whether this flow's transport holds one logical resolver token for its whole life.
    pub(crate) resolver: bool,
}

/// Why a flow could not be prepared. Reported by the caller, which owns the vocabulary for it.
#[derive(Debug)]
pub(crate) enum Denied {
    /// The aggregate would not grant this flow's composite charge.
    Grant,
    /// The client-side stack will not intercept this destination.
    Listen(ListenError),
    /// The identity space is exhausted, which is a flow that must not exist.
    Identity,
}

/// One prepared flow, before the record that will hold it exists.
///
/// Every field is something [release] must undo, which is why they arrive together rather than as a tuple the
/// caller reassembles.
pub(crate) struct Prepared {
    /// The composite grant: the record, the descriptor and every byte of this flow's buffers.
    pub(crate) connection: Connection,
    /// The client-side socket's slot in the engine's set.
    pub(crate) handle: SocketHandle,
    /// The worker identity this flow's task will run under. Issued here because a flow that cannot have one
    /// must not have a socket either.
    pub(crate) identity: Identity,
    /// The producer's end: where upstream payload is queued, and the acknowledgment a DNS-over-TCP transport
    /// waits on.
    pub(crate) mailbox: Mailbox<SocketHandle, Owned>,
    /// The engine's end of both directions: the queue it takes payload out of, and its reserved slot in the
    /// queue the task reads from. Each of them is its own wake - see [vpnhotspotd::shared::transfer].
    pub(crate) transfer: Transfer<Owned>,
    pub(crate) consumed: mpsc::Sender<()>,
    /// Client-to-upstream payload, at the end the flow's task reads it from. Counted from the moment the
    /// engine copies it out of the stack's receive buffer to the moment its consumer drops it, because that
    /// whole span is one chunk of this flow's grant.
    pub(crate) receiver: mpsc::Receiver<Owned>,
    /// The owner's half of this flow's DNS control pair: where it answers the transport, and where it takes
    /// a filled query back. Built here, after the grant, so the reusable control channels a query needs are
    /// covered by the flow's own fixed lease rather than allocated per question.
    pub(crate) serving: Serving,
    /// The transport's half of the same pair.
    pub(crate) control: mpsc::Receiver<Control>,
    pub(crate) filled: mpsc::Sender<Owned>,
}

/// What is left of a flow that will not exist, in whatever state its registration reached.
///
/// The mailbox and the upstream receiver are optional because a worker future takes both when one is started;
/// an unwind after that point drops what is left rather than pretending to hold what the future already owns.
pub(crate) struct Leftovers {
    pub(crate) connection: Connection,
    pub(crate) mailbox: Option<Mailbox<SocketHandle, Owned>>,
    pub(crate) receiver: Option<mpsc::Receiver<Owned>>,
    pub(crate) transfer: Transfer<Owned>,
    pub(crate) consumed: mpsc::Sender<()>,
    /// The transport's halves of the control pair, absent once a worker future has taken them.
    pub(crate) control: Option<mpsc::Receiver<Control>>,
    pub(crate) filled: Option<mpsc::Sender<Owned>>,
}

/// Takes everything one flow needs, in the order that lets each step undo the ones before it.
///
/// The grant first, because it is what says this flow may exist at all; then the socket, whose buffers the
/// grant already covers; then the identity, because an identity is worth nothing without somewhere to run.
/// Each failure below unwinds exactly what preceded it and leaves the set, the table and the aggregate as
/// they were.
pub(crate) fn prepare<R>(
    admission: &mut Admission,
    sockets: &mut Sockets,
    workers: &mut Workers<SocketHandle, R>,
    sizing: Sizing,
    endpoint: IpListenEndpoint,
) -> Result<Prepared, Denied> {
    // One composite grant, and it is taken before a socket buffer, a channel, an identity or a worker exists.
    // It covers the flow's record and upstream descriptor, its two stack buffers, the read scratch that is
    // really live at once, every one of the five per-flow channels built below - and, for a DNS-over-TCP
    // flow, the one logical resolver token its transport holds for its whole life. One token per *transport*,
    // not one per query: this flow's tasks cannot reach the accounting to ask per message, so the token is
    // taken here or the flow is refused.
    //
    // What it does *not* own is an exchange's worth of bytes. An idle connection has no query, no answer and
    // nothing to frame; those belong to the debt each actually submitted query takes, which is also what
    // keeps them charged when a transport closes over a question still in flight.
    let Ok(connection) = dns_debt::open(admission, sizing.bytes, sizing.resolver) else {
        return Err(Denied::Grant);
    };
    let mut socket = Socket::new(
        SocketBuffer::new(vec![0u8; sizing.flow.buffer]),
        SocketBuffer::new(vec![0u8; sizing.flow.buffer]),
    );
    socket.set_hop_limit(Some(sizing.hop_limit));
    if let Err(e) = socket.listen(endpoint) {
        // Never added to the set, so dropping it here is the whole of its cleanup.
        drop(socket);
        close_idle(admission, connection);
        return Err(Denied::Listen(e));
    }
    let handle = sockets.add(socket);
    // An identity that cannot be issued is a flow that must not exist. The socket and the grant are already
    // in hand, so this unwinds them here rather than leaving the transaction to do it.
    let Ok(identity) = workers.identity() else {
        sockets.remove(handle);
        close_idle(admission, connection);
        return Err(Denied::Identity);
    };
    // The depths that were *charged*, not the ones requested. A derived bound of zero is still a channel that
    // gets built - a zero-capacity one is not constructible - and [built_depth] is what
    // [vpnhotspotd::shared::flow_budget::channels_footprint] read when this flow's grant was taken, so
    // reading it again here is what keeps the construction inside the charge rather than beside it.
    let control = built_depth(sizing.flow.control);
    // The one queue built deeper than the rest: what the upstream half may read ahead of the client's stack.
    let read_ahead = built_depth(sizing.flow.read_ahead);
    let (downstream, receiver) = mpsc::channel(control);
    let (chunks, incoming) = mpsc::channel(read_ahead);
    let (consumed, acknowledged) = mpsc::channel(control);
    // The reusable control pair every question on this flow travels over, built once with the flow. A
    // oneshot per query would be heap that appeared before the query's own grant did, which is the shape the
    // aggregate exists to prevent.
    let (answers, control_end) = mpsc::channel(control);
    let (filled, accepted) = mpsc::channel(control);
    let mailbox = Mailbox {
        chunks,
        consumed: acknowledged,
        identity: FlowId::new(handle, identity.id),
    };
    Ok(Prepared {
        connection,
        handle,
        identity,
        mailbox,
        // Both of the engine's ends together, because they are one ownership: the queue it takes payload out
        // of, and the slot it hands chunks over through. The slot is wrapped here rather than by the caller,
        // so the only sending half this flow ever has is the one that registers the engine for a wake.
        transfer: Transfer::new(incoming, Room::new(downstream)),
        consumed,
        receiver,
        serving: Serving::new(answers, accepted),
        control: control_end,
        filled,
    })
}

/// Closes a connection that never had a question outstanding.
///
/// [dns_debt::close] can only refuse when it was asked to hand a token to a query, and none of the callers
/// here has one: the flow does not exist yet. Reported rather than discarded, because the alternative to
/// saying so is a logical token this session goes on believing it has.
fn close_idle(admission: &mut Admission, connection: Connection) {
    if dns_debt::close(admission, connection, None).is_err() {
        report::message(
            "shizuku.tcp_flow_setup",
            "a flow that never asked anything could not release its logical token",
            "Stranded",
        );
    }
}

/// The reverse of [prepare], explicitly and in reverse order.
///
/// The socket leaves the set with its two buffers, then the mailbox and every channel end, and only then is
/// the grant released - so the aggregate never reads as free while a buffer this daemon still owns is alive.
pub(crate) fn release(
    admission: &mut Admission,
    sockets: &mut Sockets,
    handle: SocketHandle,
    leftovers: Leftovers,
) {
    sockets.remove(handle);
    let Leftovers {
        connection,
        mailbox,
        receiver,
        transfer,
        consumed,
        control,
        filled,
    } = leftovers;
    drop(mailbox);
    drop(receiver);
    // Both of the engine's ends, and with them whatever the queue toward the client still held and the
    // reservation the engine was keeping in the queue toward the task.
    drop(transfer);
    drop(consumed);
    drop(control);
    drop(filled);
    close_idle(admission, connection);
}
