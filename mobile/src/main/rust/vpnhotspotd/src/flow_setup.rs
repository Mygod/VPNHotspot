//! Every resource one intercepted TCP flow needs before it may exist, and the exact reverse of acquiring
//! them.
//!
//! Here rather than beside the engine because this is the part a host can check: a lease, a socket with two
//! buffers in a real set, five bounded channels and a worker identity, taken in an order where each step can
//! undo the ones before it. The engine keeps what it cannot share - the record it stores, the future it
//! spawns, the reports it writes - and calls these two functions for the rest.
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

use crate::mailbox::{Chunk, Mailbox, Marker};
use crate::owned::Owned;
use crate::report;
use crate::tcp_dns::{Control, Serving};
use crate::workers::{Identity, Workers};
use vpnhotspotd::shared::admission::Admission;
use vpnhotspotd::shared::dns_debt::{self, Connection};
use vpnhotspotd::shared::reply_bound::built_depth;

/// The client-side stack's socket storage, and how much of it has ever really been allocated.
///
/// smoltcp's set is backed by a `Vec` it pushes to when no slot is free, and it exposes neither that vector's
/// length nor its capacity - so "the set never grew past what was charged for" is not a question the set can
/// be asked. It is asked here instead, at the one boundary where the vector can grow: a new slot is only
/// pushed when every existing one is occupied, so the high-water mark of occupancy *is* the backing length.
/// Nothing is inferred from handles, which name slots and say nothing about how many exist.
pub(crate) struct Sockets {
    set: SocketSet<'static>,
    /// How many slots the backing vector holds, which is what the engine was charged for.
    #[cfg(test)]
    pub(crate) slots: usize,
    #[cfg(test)]
    occupied: usize,
}

impl Sockets {
    pub(crate) fn new(prepared: usize) -> Self {
        Self {
            set: SocketSet::new(Vec::with_capacity(prepared)),
            #[cfg(test)]
            slots: 0,
            #[cfg(test)]
            occupied: 0,
        }
    }

    /// Adds one socket, pushing a slot only when none is free - which is exactly when the backing vector
    /// grows.
    fn add(&mut self, socket: Socket<'static>) -> SocketHandle {
        #[cfg(test)]
        {
            self.occupied += 1;
            self.slots = self.slots.max(self.occupied);
        }
        self.set.add(socket)
    }

    pub(crate) fn remove(&mut self, handle: SocketHandle) -> smoltcp::socket::Socket<'static> {
        #[cfg(test)]
        {
            self.occupied -= 1;
        }
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

/// How large the pieces of one flow are. The engine's, so a test cannot quietly agree with itself about a
/// size the daemon does not use.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sizing {
    /// What the composite grant covers: both stack buffers, the read scratch that is really live at once,
    /// and every one of the per-flow channels built below. Derived by the engine, which owns the one figure
    /// the solver and this reservation both read - see [crate::tcp]'s per-flow footprint.
    pub(crate) bytes: u64,
    /// One client-side stack buffer, each way.
    pub(crate) buffer: usize,
    /// Depth of each of the five per-flow channels.
    pub(crate) depth: usize,
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
    /// The producer's end: where upstream payload goes, and how the engine is told which flow has some.
    pub(crate) mailbox: Mailbox<SocketHandle>,
    /// The engine's end of the same mailbox.
    pub(crate) incoming: mpsc::Receiver<Chunk>,
    pub(crate) consumed: mpsc::Sender<()>,
    /// Client-to-upstream payload, and the end the flow's task reads it from. Counted from the moment the
    /// engine copies it out of the stack's receive buffer to the moment its consumer drops it, because that
    /// whole span is one chunk of this flow's grant.
    pub(crate) downstream: mpsc::Sender<Owned>,
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
    pub(crate) mailbox: Option<Mailbox<SocketHandle>>,
    pub(crate) receiver: Option<mpsc::Receiver<Owned>>,
    pub(crate) incoming: mpsc::Receiver<Chunk>,
    pub(crate) consumed: mpsc::Sender<()>,
    pub(crate) downstream: Option<mpsc::Sender<Owned>>,
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
    ready: &mpsc::Sender<Marker<SocketHandle>>,
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
        SocketBuffer::new(vec![0u8; sizing.buffer]),
        SocketBuffer::new(vec![0u8; sizing.buffer]),
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
    let (downstream, receiver) = mpsc::channel(sizing.depth);
    let (chunks, incoming) = mpsc::channel(sizing.depth);
    let (consumed, acknowledged) = mpsc::channel(sizing.depth);
    // The reusable control pair every question on this flow travels over, built once with the flow. A
    // oneshot per query would be heap that appeared before the query's own grant did, which is the shape the
    // aggregate exists to prevent.
    let (answers, control) = mpsc::channel(sizing.depth);
    let (filled, accepted) = mpsc::channel(sizing.depth);
    let mailbox = Mailbox {
        chunks,
        ready: ready.clone(),
        consumed: acknowledged,
        identity: Marker {
            handle,
            worker: identity.id,
        },
    };
    Ok(Prepared {
        connection,
        handle,
        identity,
        mailbox,
        incoming,
        consumed,
        downstream,
        receiver,
        serving: Serving::new(answers, accepted),
        control,
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
        incoming,
        consumed,
        downstream,
        control,
        filled,
    } = leftovers;
    drop(mailbox);
    drop(receiver);
    drop(incoming);
    drop(consumed);
    drop(downstream);
    drop(control);
    drop(filled);
    close_idle(admission, connection);
}

/// One readiness slot per flow the engine is prepared for, which is exactly how many identities can be ready
/// at once: the fair queue keeps at most one marker per identity, so nothing beyond that can reach the
/// channel. At least one, because a zero-capacity channel is not constructible - and that minimum is charged
/// rather than assumed free.
pub(crate) fn ready_depth(flows: usize) -> usize {
    built_depth(flows)
}
