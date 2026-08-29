use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::tcp::{ListenError, Socket, SocketBuffer};
use smoltcp::wire::IpListenEndpoint;
use tokio::sync::mpsc;

use crate::shizuku::owned::Owned;
use crate::shizuku::tcp_dns::{Control, Serving};
use vpnhotspotd::shared::admission::{Admission, Class, Lease, Request};
use vpnhotspotd::shared::bridge::{self, Bridge, Worker};
use vpnhotspotd::shared::flow_budget;
use vpnhotspotd::shared::reply_bound::built_depth;
use vpnhotspotd::shared::workers::{Identity, Workers};

/// The client-side stack's socket storage, and how much of it has ever really been allocated.
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
    /// Composite charge for the stack buffers, bridge, scratch, and per-flow channels.
    pub(crate) bytes: u64,
    /// The one value both the charge above and the channels below are sized from.
    pub(crate) flow: flow_budget::Sizing,
    /// The hop limit replies inherit, decided by the NAT66 rules rather than here.
    pub(crate) hop_limit: u8,
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
pub(crate) struct Prepared {
    /// Covers this flow's record, buffers and channels; submitted DNS queries have separate leases and may
    /// outlive it.
    pub(crate) lease: Lease,
    /// The client-side socket's slot in the engine's set.
    pub(crate) handle: SocketHandle,
    /// The identity this flow's record and its transport task share, which is also the incarnation that goes
    /// on naming the flow once that task has completed. Issued here because a flow that cannot have one must
    /// not have a socket either.
    pub(crate) identity: Identity,
    /// Engine half of the bounded Tokio bridge.
    pub(crate) bridge: Bridge,
    /// The worker's whole side of the same bridge, which is what its bidirectional copy - or its DNS-over-TCP
    /// framing - runs against.
    pub(crate) stream: Worker,
    /// The owner's half of this flow's DNS control pair: where it answers the transport, and where it takes
    /// a filled query back. Built here, after the grant, so the reusable control channels a query needs are
    /// covered by the flow's own fixed lease rather than allocated per question.
    pub(crate) serving: Serving,
    /// The transport's half of the same pair.
    pub(crate) control: mpsc::Receiver<Control>,
    pub(crate) filled: mpsc::Sender<Owned>,
}

/// What is left of a flow that will not exist, in whatever state its registration reached.
pub(crate) struct Leftovers {
    pub(crate) lease: Lease,
    pub(crate) bridge: Bridge,
    /// The worker's side of the bridge, absent once a worker future has taken it.
    pub(crate) stream: Option<Worker>,
    /// The transport's halves of the control pair, absent once a worker future has taken them.
    pub(crate) control: Option<mpsc::Receiver<Control>>,
    pub(crate) filled: Option<mpsc::Sender<Owned>>,
}

/// Takes everything one flow needs, in the order that lets each step undo the ones before it.
pub(crate) fn prepare<R>(
    admission: &mut Admission,
    sockets: &mut Sockets,
    workers: &mut Workers<SocketHandle, R>,
    sizing: Sizing,
    endpoint: IpListenEndpoint,
) -> Result<Prepared, Denied> {
    // Take the composite grant before allocating buffers, channels, an identity, or a worker.
    let Ok(lease) = admission.reserve(Request {
        records: 1,
        record_class: Class::General,
        bytes: sizing.bytes,
        byte_class: Class::General,
        ..Request::default()
    }) else {
        return Err(Denied::Grant);
    };
    let mut socket = Socket::new(
        SocketBuffer::new(vec![0u8; sizing.flow.buffer]),
        SocketBuffer::new(vec![0u8; sizing.flow.buffer]),
    );
    // The reserved tail is that receive buffer, asked of the socket - see [bridge::TailCapacity]. There is
    // no second figure to keep it in step with: `flow_budget::tail_bytes` derives the *charge* from the same
    // buffer this socket was built at, so the two cannot drift.
    let tail = bridge::TailCapacity::of(&socket);
    socket.set_hop_limit(Some(sizing.hop_limit));
    if let Err(e) = socket.listen(endpoint) {
        // Never added to the set, so dropping it here is the whole of its cleanup.
        drop(socket);
        admission.release(lease);
        return Err(Denied::Listen(e));
    }
    let handle = sockets.add(socket);
    // An identity that cannot be issued is a flow that must not exist. The socket and the grant are already
    // in hand, so this unwinds them here rather than leaving the transaction to do it.
    let Ok(identity) = workers.identity() else {
        sockets.remove(handle);
        admission.release(lease);
        return Err(Denied::Identity);
    };
    // The depths that were *charged*, not the ones requested. A derived bound of zero is still a channel that
    // gets built - a zero-capacity one is not constructible - and [built_depth] is what
    // [vpnhotspotd::shared::flow_budget::channels_footprint] read when this flow's grant was taken, so
    // reading it again here is what keeps the construction inside the charge rather than beside it.
    let control = built_depth(sizing.flow.control);
    // The whole of this flow's byte movement, at the capacities the charge above was computed from: a
    // bounded stream pair for steady-state traffic and the reserved tail the client's ending is extracted
    // into. Nothing of ours travels on either: the backpressure is the buffers filling and every wake is the
    // library's own, which is why there is no readiness channel and no acknowledgment here to build.
    let (bridge, stream) = bridge::bridge(sizing.flow.bridge, tail);
    // The reusable control pair every question on this flow travels over, built once with the flow. A
    // oneshot per query would be heap that appeared before the query's own grant did, which is the shape the
    // aggregate exists to prevent.
    let (answers, control_end) = mpsc::channel(control);
    let (filled, accepted) = mpsc::channel(control);
    Ok(Prepared {
        lease,
        handle,
        identity,
        bridge,
        stream,
        serving: Serving::new(answers, accepted),
        control: control_end,
        filled,
    })
}

/// The reverse of [prepare], explicitly and in reverse order.
pub(crate) fn release(
    admission: &mut Admission,
    sockets: &mut Sockets,
    handle: SocketHandle,
    leftovers: Leftovers,
) {
    sockets.remove(handle);
    let Leftovers {
        lease,
        bridge,
        stream,
        control,
        filled,
    } = leftovers;
    // Both halves of the bridge, and with them whatever either direction still held: an abortive ending
    // discards it, which is what a reset means.
    drop(bridge);
    drop(stream);
    drop(control);
    drop(filled);
    admission.release(lease);
}
