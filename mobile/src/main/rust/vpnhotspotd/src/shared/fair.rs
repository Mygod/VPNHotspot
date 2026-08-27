//! Per-flow bounded output ownership: whose payload is waiting, whose turn it is, and which identity a
//! signal is actually about.
//!
//! # Why per-flow and not one queue
//!
//! A single payload queue in front of many flows is head-of-line blocking however deep it is. Depth one makes
//! it worse rather than better: one flow whose peer has stopped reading holds the only slot, and every other
//! flow waits behind a chunk that belongs to none of them. A per-flow *map* beside a global queue is the same
//! thing with extra bookkeeping - the global queue is still the thing that blocks.
//!
//! So the payload lives with the flow: one queue each, sized to the same read quantum the producer uses, and
//! what this owns is the *order* those flows are served in. The markers in that order are the owner's own
//! notes to self - nothing travels between a flow's producer and its owner to say work is waiting, because
//! each flow's queue is what wakes the owner (see [crate::shared::transfer]) - so duplicates coalesce here,
//! and what to do about a marker is decided by inspecting owner state rather than by trusting it.
//!
//! # The row is one chunk, whatever the producer is allowed to queue
//!
//! What a flow owes here is exactly one chunk, at an exact offset: the one being written. Partial writes keep
//! that offset and rotate the flow to the back of the order, and only a chunk that is entirely gone frees the
//! row. What happens when it does is the producer's business rather than this module's - an ordinary producer
//! has more queued and the owner refills the row from that queue, while an acknowledged one is told the piece
//! was consumed and only then builds its next. Both are in [crate::shared::mailbox]; neither may put a second
//! chunk in the row, because a row freed on delivery would let one be written while another was still being
//! sent.
//!
//! # Identity, because handles are reused
//!
//! smoltcp hands back socket handles, so a handle alone names a slot rather than a flow: a terminal from a
//! closed flow and a row belonging to the flow that reused its handle are indistinguishable by handle. Every
//! operation here therefore takes the retained worker identity beside the handle and validates the pair. A
//! stale identity is discarded on sight and - the part that matters - cannot suppress the successor's
//! readiness, because dedup is keyed on the pair rather than on the handle.
//!
//! Nothing here is async. What it owns is the decision; the waiting, the sending and the task lifecycle
//! belong to the ingress owner that drives it.

use std::collections::{HashMap, VecDeque};

use crate::shared::admission::{linear_footprint, logical_footprint};

/// One flow, named by the pair that actually identifies it.
///
/// The worker id is the retained identity the task registry issued, so it is unique for the life of the
/// process rather than for the life of a slot.
/// `H` is whatever the owner's transport names a slot by - a smoltcp `SocketHandle` for the TCP engine - and
/// is a parameter rather than a number so that nothing here has to convert one, and so that this module stays
/// free of the transport it serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FlowId<H> {
    pub handle: H,
    pub worker: u64,
}

impl<H> FlowId<H> {
    pub fn new(handle: H, worker: u64) -> Self {
        Self { handle, worker }
    }
}

/// What one flow owes the wire, owner-confined: the chunk, how much of it has gone, and whether the ordered
/// end-of-stream is still to follow.
#[derive(Debug)]
struct Pending<P> {
    payload: Option<Chunk<P>>,
    /// Set once the producer has signalled end of stream. Delivered strictly after the payload, because a
    /// half-close that overtook the bytes before it would truncate the stream.
    eof: bool,
    /// The flow is being retired: its state has been discarded and nothing more may be admitted for it.
    retiring: bool,
    /// Whether this identity already has a marker in [FairQueue::order], so a flood of wakes coalesces into
    /// one instead of growing the queue.
    ///
    /// A field on the row rather than a second hash container keyed by the same identity, and that is a
    /// correctness property rather than a saving. A wake arrives *after* the owner has taken ownership of a
    /// payload, so there is no honest way to refuse one: a separate index that had run out of room would
    /// leave bytes in a queue nobody would ever be told to send. Living in the row the flow already has,
    /// this cannot run out - the row's own admission is the only capacity decision.
    queued: bool,
}

impl<P> Default for Pending<P> {
    fn default() -> Self {
        Self {
            payload: None,
            eof: false,
            retiring: false,
            queued: false,
        }
    }
}

#[derive(Debug)]
struct Chunk<P> {
    bytes: P,
    offset: usize,
}

/// What a service attempt did, which is what the caller needs to know to decide what the row does next: take
/// the flow's next queued chunk, tell an acknowledged producer, or leave the offset where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// The whole chunk went, so the row is free for whatever this flow has queued behind it. Not the
    /// producer's permission to read on - that is its own queue's room, and the owner took a chunk out of it
    /// to fill this row - except for the acknowledged handover DNS-over-TCP uses, which waits for exactly
    /// this.
    Consumed,
    /// Some of it went, or none of it did. The exact offset is kept and this flow rotates to the back.
    Blocked,
    /// Every byte was consumed and the ordered end of stream has now been delivered too.
    Eof,
    /// Nothing to do for this identity: no payload, no pending EOF, or it is not a flow this owns.
    Idle,
}

/// Why a delivery was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// The row still holds a chunk the wire has not fully consumed. Nothing more may be delivered into it;
    /// whatever the producer has queued behind it waits in its own queue.
    Occupied,
    /// This identity is retiring or was never registered. Stale, and dropped rather than queued.
    Unknown,
}

/// One pass over the ready set.
///
/// The budget is taken when the round begins, so a flow that rotates to the back is not serviced twice in the
/// same round and a flow that keeps signalling readiness cannot extend it. That is the whole of the fairness
/// guarantee: every flow that was ready when the round started is offered its turn before any flow gets a
/// second one.
#[derive(Debug)]
pub struct Round {
    budget: usize,
}

/// Registering a flow would exceed the fixed bound charged through [FairQueue::footprint].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtCapacity {
    pub prepared: usize,
}

/// The ready order and every flow's owner-confined pending state.
///
/// Bounded by a **logical maximum**: `prepared` live identities, which is the number [FairQueue::footprint]
/// charged row state for and the one condition [FairQueue::admit] refuses on. Both collections are built with
/// `with_capacity(prepared)` so the common case allocates nothing, but that is an optimisation rather than the
/// safety property - the hash map's own backing is opaque count-bounded overhead under this policy, free to
/// reorganise as it likes.
///
/// Removal is the other half, and it is ordinary: a retirement frees a logical slot and the next identity may
/// take it. The charge is for the bound rather than for what is in it, so it is retained whatever leaves and
/// given back only when the whole queue is rebuilt or dropped.
#[derive(Debug)]
pub struct FairQueue<H, P = Vec<u8>> {
    flows: HashMap<FlowId<H>, Pending<P>>,
    /// Explicit round-robin order. A `HashMap` iteration would be an arbitrary order that changes when the
    /// map is resized, which is not fairness - it is a different unfairness each time.
    order: VecDeque<FlowId<H>>,
    /// How many live identities this was prepared for; nothing is admitted past it.
    prepared: usize,
}

/// One identity in the round-robin order, which is a `VecDeque` and so really does store the bare type.
fn marker_bytes<H>() -> u64 {
    std::mem::size_of::<FlowId<H>>() as u64
}

impl<H: Copy + Eq + std::hash::Hash, P: std::ops::Deref<Target = [u8]>> FairQueue<H, P> {
    /// Prepares for `flows` live identities: the logical maximum [FairQueue::footprint] charged row state for
    /// and the one thing [FairQueue::admit] refuses on.
    pub fn with_capacity(flows: usize) -> Self {
        Self {
            // Requested at that maximum so the common case allocates nothing. What the map really allocates,
            // and when it reorganises it, is its own business - count-bounded overhead here, not a
            // measurement and not a promise.
            flows: HashMap::with_capacity(flows),
            order: VecDeque::with_capacity(flows),
            prepared: flows,
        }
    }

    /// What a queue prepared for `flows` identities owns, whatever is in it - which is what the owner charges,
    /// once, and keeps charged until the queue is rebuilt or dropped.
    ///
    /// Two collections, each named by the type it really stores. The flow rows are
    /// [logical_footprint] - state, with the container's own backing count-bounded instead - while the
    /// round-robin order is a `VecDeque` and so is charged in full through [linear_footprint]. The payload a
    /// row points at is not here: it belongs to whoever produced it and is charged there.
    ///
    /// `None` when the arithmetic would wrap, which is a bound that cannot be charged and therefore must not
    /// be prepared.
    pub fn footprint(flows: usize) -> Option<u64> {
        logical_footprint::<(FlowId<H>, Pending<P>)>(flows)?
            .checked_add(linear_footprint(flows, marker_bytes::<H>())?)?
            .checked_add(std::mem::size_of::<Self>() as u64)
    }

    /// The logical maximum: how many live identities this queue admits at once.
    pub fn prepared(&self) -> usize {
        self.prepared
    }

    /// Registers a flow, inside the prepared bound.
    ///
    /// Idempotent for the same identity; a reused handle with a new worker id is a different flow and gets its
    /// own state. A *new* identity is refused on one condition - the prepared bound, which is the logical
    /// maximum this queue was charged row state for. Re-admitting an identity already here takes no slot, and
    /// a retired one leaves its slot free for the next.
    pub fn admit(&mut self, id: FlowId<H>) -> Result<(), AtCapacity> {
        if !self.flows.contains_key(&id) && self.flows.len() >= self.prepared {
            return Err(AtCapacity {
                prepared: self.prepared,
            });
        }
        self.flows.entry(id).or_default();
        Ok(())
    }

    pub fn is_admitted(&self, id: FlowId<H>) -> bool {
        self.flows.get(&id).is_some_and(|flow| !flow.retiring)
    }

    pub fn ready_len(&self) -> usize {
        self.order.len()
    }

    /// Whether this flow's row will take a chunk: it holds none and the flow is not retiring.
    ///
    /// A question about the row rather than about the producer. What lets the producer read on is room in its
    /// own queue, which the owner makes by taking a chunk out of it - and taking one is exactly what the
    /// owner does while this answers `true`.
    pub fn accepts(&self, id: FlowId<H>) -> bool {
        self.flows
            .get(&id)
            .is_some_and(|flow| !flow.retiring && flow.payload.is_none())
    }

    /// Hands one chunk to one flow's row, which holds one.
    ///
    /// Refused rather than queued when the row is occupied: what may be waiting for this flow is bounded by
    /// the producer's own queue, and a second chunk accepted here would be one this owner holds outside any
    /// bound.
    pub fn deliver(&mut self, id: FlowId<H>, bytes: P) -> Result<(), (P, Rejected)> {
        let Some(flow) = self.flows.get_mut(&id) else {
            return Err((bytes, Rejected::Unknown));
        };
        if flow.retiring {
            return Err((bytes, Rejected::Unknown));
        }
        if flow.payload.is_some() {
            return Err((bytes, Rejected::Occupied));
        }
        flow.payload = Some(Chunk { bytes, offset: 0 });
        self.mark_ready(id);
        Ok(())
    }

    /// Records that the producer has finished. Delivered only after the payload already in the row.
    pub fn signal_eof(&mut self, id: FlowId<H>) {
        let Some(flow) = self.flows.get_mut(&id) else {
            return;
        };
        if flow.retiring {
            return;
        }
        flow.eof = true;
        self.mark_ready(id);
    }

    /// A payload-free wake. Ignored for an identity this does not own, which is what makes a stale marker
    /// harmless - and, because the dedup key is the pair, one stale marker cannot stand in for the successor
    /// that reused the handle.
    pub fn mark_ready(&mut self, id: FlowId<H>) {
        let Some(flow) = self.flows.get_mut(&id) else {
            return;
        };
        if flow.retiring || flow.queued {
            return;
        }
        flow.queued = true;
        // One marker per live identity, and the order was prepared for the whole bound, so this cannot grow
        // it. Nothing here can fail, which is the point: a wake arrives after a payload is already owned.
        self.order.push_back(id);
    }

    /// Opens a pass over everything ready right now.
    pub fn begin_round(&self) -> Round {
        Round {
            budget: self.order.len(),
        }
    }

    /// The next flow to service in this round, or `None` when the round is over.
    pub fn next(&mut self, round: &mut Round) -> Option<FlowId<H>> {
        while round.budget > 0 {
            round.budget -= 1;
            let id = self.order.pop_front()?;
            // A marker outliving its flow is discarded here rather than acted on. The flag is cleared on the
            // row the marker names, so a flow serviced now can be marked again by the next wake.
            if let Some(flow) = self.flows.get_mut(&id) {
                flow.queued = false;
                if !flow.retiring {
                    return Some(id);
                }
            }
        }
        None
    }

    /// What this flow currently owes the wire, from its exact offset.
    pub fn peek(&self, id: FlowId<H>) -> Option<&[u8]> {
        let flow = self.flows.get(&id)?;
        if flow.retiring {
            return None;
        }
        let chunk = flow.payload.as_ref()?;
        chunk.bytes.get(chunk.offset..)
    }

    /// Records how much of the pending chunk the wire actually took.
    ///
    /// A short write is not a failure and not a loss: the offset advances by exactly what went, the rest stays
    /// where it is, and the flow rotates to the back so the flows behind it get their turn first. Only a chunk
    /// that is entirely gone frees the row.
    pub fn serviced(&mut self, id: FlowId<H>, sent: usize) -> Progress {
        let Some(flow) = self.flows.get_mut(&id) else {
            return Progress::Idle;
        };
        if flow.retiring {
            return Progress::Idle;
        }
        match flow.payload.as_mut() {
            Some(chunk) => {
                chunk.offset = chunk.offset.saturating_add(sent).min(chunk.bytes.len());
                if chunk.offset < chunk.bytes.len() {
                    // Still owed. Back of the order, so a peer that has stopped reading cannot hold the turn.
                    self.mark_ready(id);
                    return Progress::Blocked;
                }
                flow.payload = None;
                if flow.eof {
                    flow.eof = false;
                    return Progress::Eof;
                }
                Progress::Consumed
            }
            // No payload left, so an end of stream signalled earlier is now in order.
            None if flow.eof => {
                flow.eof = false;
                Progress::Eof
            }
            None => Progress::Idle,
        }
    }

    /// Whether this flow's row still holds anything for the wire: payload not yet taken, or an ordered end
    /// of stream not yet delivered, on an admitted row that is not retiring.
    ///
    /// A report rather than a gate. Nothing waits on it - a clean terminal detaches the flow and its queued
    /// delivery carries on (see [crate::shared::transfer::dispose]), and an ending discards the row rather
    /// than waiting for it. What this answers is what such a decision would be discarding.
    pub fn owes(&self, id: FlowId<H>) -> bool {
        self.flows
            .get(&id)
            .is_some_and(|flow| !flow.retiring && (flow.payload.is_some() || flow.eof))
    }

    /// Discards this exact identity's payload, ordered EOF and queued marker, and answers with the payload so
    /// the caller drops it rather than this module.
    ///
    /// First, and before the worker is cancelled: cancellation may bypass a handover wait - for room in the
    /// producer's queue, or for the acknowledgment an acknowledged handover waits on - only once the owner has
    /// committed to discarding what that wait was for. The reverse order is a worker released while the owner
    /// still believes it owes bytes.
    pub fn begin_retire(&mut self, id: FlowId<H>) -> Option<P> {
        let flow = self.flows.get_mut(&id)?;
        flow.retiring = true;
        flow.eof = false;
        let discarded = flow.payload.take().map(|chunk| chunk.bytes);
        if std::mem::take(&mut flow.queued) {
            self.order.retain(|marker| *marker != id);
        }
        discarded
    }

    /// Forgets the flow entirely, after its worker has been joined and its resources dropped.
    pub fn finish_retire(&mut self, id: FlowId<H>) {
        // The flag goes with the row, so what is left to do is take the marker out of the order it named.
        if self.flows.remove(&id).is_some_and(|flow| flow.queued) {
            self.order.retain(|marker| *marker != id);
        }
    }

    pub fn len(&self) -> usize {
        self.flows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.flows.is_empty()
    }
}

/// Everything one flow admission needs to build, and everything it needs to undo.
///
/// A trait rather than a closure so the *sequence* below is production's rather than each caller's: the
/// engine's implementation opens a smoltcp socket, takes a charged grant, builds the flow's two queues, and
/// hands the record to the worker table. Which of those exists at each step is the
/// property under test, and a caller that inlined the sequence would be a second copy of it.
pub trait FlowOps {
    /// Names one flow's transport slot. smoltcp's `SocketHandle` in production.
    type Handle: Copy + Eq + std::hash::Hash;
    /// The record the worker table takes, and what an unwind has to give back.
    type Record;
    /// Why a build failed, which the caller reports its own way.
    type Error;
    /// What one chunk of this flow's payload is carried as. `Vec<u8>` in every owner that has no reason to
    /// wrap it.
    type Payload: std::ops::Deref<Target = [u8]>;

    /// Whether the worker table can take another record without growing. Asked *before* anything is built,
    /// so a refusal costs nothing to unwind.
    fn has_room(&self) -> bool;

    /// Opens the socket, takes the grant, builds the channels, and answers the identity they belong to.
    fn build(&mut self) -> Result<(Self::Handle, u64, Self::Record), Self::Error>;

    /// Undoes [FlowOps::build] exactly: the socket, the channels and the grant all go.
    fn unwind(&mut self, handle: Self::Handle, worker: u64, record: Self::Record);

    /// Hands the record to the worker table and starts its task. The record comes back on refusal.
    fn admit(
        &mut self,
        handle: Self::Handle,
        worker: u64,
        record: Self::Record,
    ) -> Result<(), Self::Record>;
}

/// Why a flow could not be admitted.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused<E> {
    /// One of the prepared tables is full. Answered before anything was built.
    AtCapacity(AtCapacity),
    /// The socket, the grant or the channels could not be made.
    Unbuildable(E),
}

/// Admits one flow, or leaves nothing behind.
///
/// The order is the whole of it, and every step of it was wrong at some point:
///
/// 1. **capacity first**, in *both* tables, before a descriptor is opened or a byte is charged - a refusal
///    that arrives after the socket is open is a socket to close and a grant to release, on a path that only
///    runs when the daemon is already at its limit;
/// 2. **build** the socket, the grant and the channels;
/// 3. **register** with the fair queue and the round-robin order, together or not at all - a flow in one and
///    not the other is one whose payload nothing would ever service;
/// 4. **admit** to the worker table, which starts the task.
///
/// Any failure after step 2 unwinds everything built, in the reverse order, so there is no socket, no task,
/// no queue and no grant left over.
pub fn admit_flow<O: FlowOps>(
    ops: &mut O,
    fair: &mut FairQueue<O::Handle, O::Payload>,
    order: &mut VecDeque<O::Handle>,
    prepared: usize,
) -> Result<O::Handle, Refused<O::Error>> {
    // Both tables, before anything exists to unwind.
    if !ops.has_room() || fair.len() >= prepared || order.len() >= prepared {
        return Err(Refused::AtCapacity(AtCapacity { prepared }));
    }
    let (handle, worker, record) = ops.build().map_err(Refused::Unbuildable)?;
    let id = FlowId::new(handle, worker);
    if let Err(why) = register(fair, order, prepared, id) {
        // Unreachable while the check above holds, and unwound anyway: the alternative is a descriptor
        // nothing closes and a grant nothing releases.
        ops.unwind(handle, worker, record);
        return Err(Refused::AtCapacity(why));
    }
    if let Err(record) = ops.admit(handle, worker, record) {
        deregister(fair, order, id);
        ops.unwind(handle, worker, record);
        return Err(Refused::AtCapacity(AtCapacity { prepared }));
    }
    Ok(handle)
}

/// Registers one flow with both of the collections that index it, or with neither.
///
/// Transactional, and that is the whole reason it is a function rather than two calls at the call site: the
/// caller has a socket, a queue and a charged grant in hand by the time it gets here, and a half-registered
/// flow is one whose payload nothing would ever service. Either both take it, or the one that took it gives
/// it back and the caller unwinds.
///
/// Neither collection may grow. Both were prepared to `prepared` and charged for exactly that, so admitting
/// past it is an allocation the aggregate does not know about - refused here rather than done.
pub fn register<H: Copy + Eq + std::hash::Hash, P: std::ops::Deref<Target = [u8]>>(
    fair: &mut FairQueue<H, P>,
    order: &mut VecDeque<H>,
    prepared: usize,
    id: FlowId<H>,
) -> Result<(), AtCapacity> {
    if order.len() >= prepared {
        // Checked first, because it is the half with no undo cost: nothing has been registered yet.
        return Err(AtCapacity { prepared });
    }
    fair.admit(id)?;
    order.push_back(id.handle);
    Ok(())
}

/// Forgets one flow from both, in the order a retirement needs: its payload and marker are discarded before
/// its place in the round-robin goes.
pub fn deregister<H: Copy + Eq + std::hash::Hash, P: std::ops::Deref<Target = [u8]>>(
    fair: &mut FairQueue<H, P>,
    order: &mut VecDeque<H>,
    id: FlowId<H>,
) -> Option<P> {
    let discarded = fair.begin_retire(id);
    fair.finish_retire(id);
    order.retain(|queued| *queued != id.handle);
    discarded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a() -> FlowId<u32> {
        FlowId::new(1, 100)
    }

    fn b() -> FlowId<u32> {
        FlowId::new(2, 200)
    }

    /// One pass, servicing everything that was ready when it began.
    fn drain(
        queue: &mut FairQueue<u32>,
        sent: impl Fn(FlowId<u32>) -> usize,
    ) -> Vec<(FlowId<u32>, Progress)> {
        let mut round = queue.begin_round();
        let mut serviced = Vec::new();
        while let Some(id) = queue.next(&mut round) {
            let took = sent(id);
            serviced.push((id, queue.serviced(id, took)));
        }
        serviced
    }

    /// The head-of-line case, in the only shape that matters: A's peer never takes a byte, and B's traffic is
    /// completely unaffected.
    #[test]
    fn a_flow_that_never_drains_cannot_block_another() {
        let mut queue = FairQueue::with_capacity(8);
        queue.admit(a()).expect("prepared");
        queue.admit(b()).expect("prepared");
        queue.deliver(a(), vec![0; 8]).expect("A's row is free");
        queue.deliver(b(), vec![1; 4]).expect("B's row is free");

        // A takes nothing, ever. B takes everything.
        for _ in 0..10 {
            let serviced = drain(&mut queue, |id| if id == a() { 0 } else { usize::MAX });
            for (id, progress) in serviced {
                if id == a() {
                    assert_eq!(progress, Progress::Blocked);
                } else {
                    assert_eq!(progress, Progress::Consumed);
                }
            }
            // B's row is free again every round, so the owner can take its next chunk straight away.
            assert!(queue.accepts(b()));
            let _ = queue.deliver(b(), vec![1; 4]);
        }
        // A still owes exactly what it owed, unconsumed and unlost.
        assert_eq!(queue.peek(a()).expect("A still owes its chunk").len(), 8);
        assert!(!queue.accepts(a()));
    }

    /// A flood of wakes from one flow coalesces into one marker and cannot extend its own round.
    #[test]
    fn repeated_readiness_from_one_flow_cannot_starve_another() {
        let mut queue = FairQueue::with_capacity(8);
        queue.admit(a()).expect("prepared");
        queue.admit(b()).expect("prepared");
        queue.deliver(a(), vec![0; 8]).expect("free");
        for _ in 0..100 {
            queue.mark_ready(a());
        }
        queue.deliver(b(), vec![1; 4]).expect("free");
        // One marker each, however many wakes arrived.
        assert_eq!(queue.ready_len(), 2);
        let serviced = drain(&mut queue, |id| if id == a() { 0 } else { usize::MAX });
        // Both were offered their turn, in order, exactly once.
        assert_eq!(
            serviced,
            vec![(a(), Progress::Blocked), (b(), Progress::Consumed)]
        );
    }

    /// A short write keeps the exact offset and does not overwrite or lose what is left.
    #[test]
    fn a_partial_send_keeps_its_exact_offset() {
        let mut queue = FairQueue::with_capacity(8);
        queue.admit(a()).expect("prepared");
        queue.deliver(a(), vec![1, 2, 3, 4, 5]).expect("free");
        let mut round = queue.begin_round();
        assert_eq!(queue.next(&mut round), Some(a()));
        assert_eq!(queue.serviced(a(), 2), Progress::Blocked);
        assert_eq!(queue.peek(a()), Some(&[3, 4, 5][..]));
        // And the rest goes from exactly there.
        let mut round = queue.begin_round();
        assert_eq!(queue.next(&mut round), Some(a()));
        assert_eq!(queue.serviced(a(), 3), Progress::Consumed);
        assert_eq!(queue.peek(a()), None);
        assert!(queue.accepts(a()));
    }

    /// A row is one chunk: nothing may be delivered into it until what it holds is *consumed*, not merely
    /// delivered or partially sent.
    #[test]
    fn no_second_chunk_in_the_row_before_the_first_is_consumed() {
        let mut queue = FairQueue::with_capacity(8);
        queue.admit(a()).expect("prepared");
        queue.deliver(a(), vec![1, 2, 3, 4]).expect("free");
        assert!(!queue.accepts(a()));
        let (returned, why) = queue
            .deliver(a(), vec![9, 9])
            .expect_err("the row is occupied");
        assert_eq!(why, Rejected::Occupied);
        assert_eq!(returned, vec![9, 9]);

        // Partially sent is still occupied.
        let mut round = queue.begin_round();
        queue.next(&mut round);
        assert_eq!(queue.serviced(a(), 1), Progress::Blocked);
        assert!(!queue.accepts(a()));
        assert_eq!(
            queue.deliver(a(), vec![9]).expect_err("still occupied").1,
            Rejected::Occupied
        );

        // Only full consumption frees it.
        let mut round = queue.begin_round();
        queue.next(&mut round);
        assert_eq!(queue.serviced(a(), 3), Progress::Consumed);
        assert!(queue.accepts(a()));
        queue.deliver(a(), vec![9]).expect("free again");
    }

    /// End of stream is ordered after the bytes before it, and is per flow.
    #[test]
    fn end_of_stream_follows_the_payload_it_belongs_to() {
        let mut queue = FairQueue::with_capacity(8);
        queue.admit(a()).expect("prepared");
        queue.admit(b()).expect("prepared");
        queue.deliver(a(), vec![1, 2, 3]).expect("free");
        queue.signal_eof(a());
        // Still the payload first.
        let mut round = queue.begin_round();
        assert_eq!(queue.next(&mut round), Some(a()));
        assert_eq!(queue.serviced(a(), 1), Progress::Blocked);
        assert!(queue.owes(a()));

        // The last byte and the ordered EOF arrive together, in that order.
        let mut round = queue.begin_round();
        queue.next(&mut round);
        assert_eq!(queue.serviced(a(), 2), Progress::Eof);
        assert!(!queue.owes(a()));

        // And B's own half-close is independent of A entirely.
        queue.signal_eof(b());
        let mut round = queue.begin_round();
        assert_eq!(queue.next(&mut round), Some(b()));
        assert_eq!(queue.serviced(b(), 0), Progress::Eof);
    }

    /// A stalled flow does not delay another flow's half-close or its completion.
    #[test]
    fn a_stalled_flow_delays_neither_half_close_nor_completion() {
        let mut queue = FairQueue::with_capacity(8);
        queue.admit(a()).expect("prepared");
        queue.admit(b()).expect("prepared");
        queue.deliver(a(), vec![0; 8]).expect("free");
        queue.deliver(b(), vec![1; 2]).expect("free");
        queue.signal_eof(b());
        let serviced = drain(&mut queue, |id| if id == a() { 0 } else { usize::MAX });
        assert_eq!(
            serviced,
            vec![(a(), Progress::Blocked), (b(), Progress::Eof)]
        );
        // B owes nothing, so ending B's flow would discard nothing, while A is still owed exactly what it
        // was owed.
        assert!(!queue.owes(b()));
        assert!(queue.owes(a()));
    }

    /// A row owes until the wire has taken all of it, and the ordered end of stream after it is what
    /// finally clears it. Partially consumed is still owed.
    #[test]
    fn a_row_owes_until_the_wire_takes_it_and_the_end_of_stream_follows() {
        let mut queue = FairQueue::with_capacity(8);
        queue.admit(a()).expect("prepared");
        queue.deliver(a(), vec![1, 2, 3]).expect("free");
        queue.signal_eof(a());
        assert!(queue.owes(a()));
        // Partially consumed is still owed.
        let mut round = queue.begin_round();
        queue.next(&mut round);
        queue.serviced(a(), 1);
        assert!(queue.owes(a()));
        // Only the ordered EOF after the last byte clears it.
        let mut round = queue.begin_round();
        queue.next(&mut round);
        assert_eq!(queue.serviced(a(), 2), Progress::Eof);
        assert!(!queue.owes(a()));
    }

    /// Retirement discards this exact identity's payload, EOF and marker *before* anything cancels its worker,
    /// and hands the payload back so the owner drops it.
    #[test]
    fn retirement_discards_before_it_cancels() {
        let mut queue = FairQueue::with_capacity(8);
        queue.admit(a()).expect("prepared");
        queue.admit(b()).expect("prepared");
        queue.deliver(a(), vec![7; 3]).expect("free");
        queue.signal_eof(a());
        queue.deliver(b(), vec![1; 2]).expect("free");
        assert_eq!(queue.ready_len(), 2);

        let discarded = queue.begin_retire(a()).expect("A had a chunk");
        assert_eq!(discarded, vec![7; 3]);
        // Nothing of A's is left to act on, and B's marker is untouched.
        assert!(!queue.owes(a()));
        assert_eq!(queue.ready_len(), 1);
        assert!(!queue.accepts(a()));
        assert_eq!(
            queue.deliver(a(), vec![0]).expect_err("retiring").1,
            Rejected::Unknown
        );
        let mut round = queue.begin_round();
        assert_eq!(queue.next(&mut round), Some(b()));
        queue.finish_retire(a());
        assert_eq!(queue.len(), 1);
    }

    /// A handle is a slot, not a flow. A stale signal cannot reach the successor that reused the handle, and -
    /// the part a handle-keyed dedup gets wrong - cannot suppress that successor's own readiness either.
    #[test]
    fn a_stale_identity_cannot_touch_the_flow_that_reused_its_handle() {
        let mut queue = FairQueue::with_capacity(8);
        let stale = FlowId::new(1, 100);
        let successor = FlowId::new(1, 101);
        queue.admit(stale).expect("prepared");
        queue.deliver(stale, vec![5; 2]).expect("free");
        // The stale flow is queued and then retired and forgotten.
        queue.begin_retire(stale);
        queue.finish_retire(stale);

        queue.admit(successor).expect("prepared");
        // A late payload, EOF and readiness from the old identity all land on nothing.
        assert_eq!(
            queue.deliver(stale, vec![9]).expect_err("stale").1,
            Rejected::Unknown
        );
        queue.signal_eof(stale);
        queue.mark_ready(stale);
        assert_eq!(queue.ready_len(), 0);
        assert_eq!(queue.serviced(stale, 1), Progress::Idle);
        assert!(!queue.owes(successor));

        // And the successor's own readiness is unaffected by any of it.
        queue.deliver(successor, vec![1; 2]).expect("free");
        assert_eq!(queue.ready_len(), 1);
        let mut round = queue.begin_round();
        assert_eq!(queue.next(&mut round), Some(successor));
        assert_eq!(queue.serviced(successor, 2), Progress::Consumed);
    }

    /// A marker left behind by a flow that was retired mid-round is discarded when it comes up rather than
    /// standing in for anything.
    #[test]
    fn a_marker_outliving_its_flow_is_discarded_when_it_comes_up() {
        let mut queue = FairQueue::with_capacity(8);
        queue.admit(a()).expect("prepared");
        queue.admit(b()).expect("prepared");
        queue.deliver(a(), vec![0; 2]).expect("free");
        queue.deliver(b(), vec![1; 2]).expect("free");
        let mut round = queue.begin_round();
        // A is retired after the round began, so its marker is stale by the time it is reached.
        queue.begin_retire(a());
        // Retirement removed the marker, so the round now reaches B directly and A is never offered.
        assert_eq!(queue.next(&mut round), Some(b()));
        assert_eq!(queue.next(&mut round), None);
    }

    /// Admission stops at the prepared capacity rather than allocating past it, and the refusal names what it
    /// was prepared for so the owner can charge the growth.
    #[test]
    fn insertion_at_prepared_capacity_is_refused_rather_than_allocated() {
        let mut queue = FairQueue::with_capacity(2);
        queue.admit(a()).expect("prepared");
        queue.admit(b()).expect("prepared");
        // Re-admitting an identity already here is not growth and stays allowed.
        queue.admit(a()).expect("already present");
        assert_eq!(queue.len(), 2);
        assert_eq!(
            queue.admit(FlowId::new(3, 300)),
            Err(AtCapacity { prepared: 2 })
        );
        assert_eq!(queue.len(), 2, "a refusal registers nothing");
        // The refusal did not disturb the flows that were already here.
        queue.deliver(a(), vec![1, 2]).expect("free");
        assert!(queue.owes(a()));
    }

    /// A queue prepared for a bound admits every identity in it, however the traffic arrives: each one gets a
    /// payload and a marker, the bound refuses the next, and the readiness flag lives in the row rather than in
    /// a second collection that could run out.
    #[test]
    fn a_prepared_queue_admits_its_whole_bound() {
        let mut queue = FairQueue::with_capacity(64);
        for handle in 0..64u32 {
            let id = FlowId::new(handle, u64::from(handle) + 1_000);
            queue.admit(id).expect("prepared");
            queue.deliver(id, vec![0; 4]).expect("free");
            queue.signal_eof(id);
        }
        assert_eq!(queue.len(), 64);
        assert_eq!(queue.ready_len(), 64);
        assert_eq!(
            queue.admit(FlowId::new(64, 2_000)),
            Err(AtCapacity { prepared: 64 }),
            "and the bound is what refuses the next one"
        );
    }

    /// An emptied queue keeps its bound and takes the whole of it again, and every re-admitted flow works.
    ///
    /// Ordinary reuse: a retirement frees a logical slot, and the next identity gets it. The charge follows the
    /// prepared bound rather than the rows in it, so nothing is refunded when a flow leaves and nothing extra
    /// is owed when its successor arrives. What the flow map does with its own backing across all of that is
    /// its business - opaque count-bounded overhead under this policy, and nothing this test reads.
    #[test]
    fn an_emptied_queue_takes_its_whole_bound_again() {
        let mut queue = FairQueue::with_capacity(16);
        for handle in 0..16u32 {
            let id = FlowId::new(handle, u64::from(handle));
            queue.admit(id).expect("prepared");
            queue.deliver(id, vec![0; 2]).expect("free");
        }
        for handle in 0..16u32 {
            let id = FlowId::new(handle, u64::from(handle));
            queue.begin_retire(id);
            queue.finish_retire(id);
        }
        assert!(queue.is_empty());
        assert_eq!(queue.ready_len(), 0);
        assert_eq!(
            queue.prepared(),
            16,
            "the bound the charge covers is untouched by what left"
        );

        // The whole bound again, and one past it still refused.
        for handle in 0..16u32 {
            queue
                .admit(FlowId::new(handle, u64::from(handle) + 500))
                .expect("a retired slot is available again");
        }
        assert_eq!(queue.len(), 16);
        assert_eq!(
            queue.admit(FlowId::new(16, 9_999)),
            Err(AtCapacity { prepared: 16 }),
            "and the bound still refuses the next one"
        );
        assert_eq!(
            queue.ready_len(),
            0,
            "no marker for a flow nothing was delivered to"
        );
        // And each one is a working flow rather than a row that merely exists: its row is free, so a chunk
        // may be delivered into it, and each is announced exactly once.
        for handle in 0..16u32 {
            let id = FlowId::new(handle, u64::from(handle) + 500);
            assert!(queue.accepts(id), "flow {handle} takes a chunk");
            queue.deliver(id, vec![1; 3]).expect("free");
        }
        assert_eq!(queue.ready_len(), 16);
    }

    /// Ordinary retire-and-replace continues for the whole bound, and a wake for a live flow is never
    /// refused.
    ///
    /// The shape this owner is really in: a flow ends, its successor takes the slot, and both the payload
    /// handover and the readiness marker have to keep working across that. Readiness is a flag on the row
    /// rather than a second collection keyed by the same identity, which is what makes the second half
    /// unconditional - a wake arrives after the owner already owns a payload, so there is no honest way to
    /// refuse one, and nothing here has to.
    ///
    /// Repeated for the whole bound, because that is the shape a long-lived session is in: a flow ends, its
    /// slot goes back to the bound, and the next identity takes it. Nothing consults the container in between,
    /// because a retirement frees a *logical* slot and how the map arranges its own backing across that is
    /// opaque count-bounded overhead this owner has no opinion about.
    #[test]
    fn flows_retire_and_are_replaced_without_stranding_a_wake() {
        let prepared = 8usize;
        let mut queue: FairQueue<u32> = FairQueue::with_capacity(prepared);
        let mut worker = 0u64;
        let mut live: Vec<FlowId<u32>> = Vec::new();
        while live.len() < prepared {
            worker += 1;
            let id = FlowId::new(live.len() as u32, worker);
            queue.admit(id).expect("prepared");
            queue.deliver(id, vec![0u8; 4]).expect("free");
            live.push(id);
        }
        assert_eq!(queue.ready_len(), prepared, "every payload was announced");

        for _ in 0..prepared {
            let retiring = live.remove(0);
            queue.begin_retire(retiring);
            queue.finish_retire(retiring);
            assert_eq!(
                queue.len(),
                live.len(),
                "the retired flow's row and marker both went with it"
            );
            worker += 1;
            // The successor reuses the handle its predecessor gave up, which is what a reused smoltcp slot
            // looks like here.
            let successor = FlowId::new(retiring.handle, worker);
            queue
                .admit(successor)
                .expect("a retired flow leaves its slot for the next");
            // A wake follows a payload the owner already holds, so this one cannot be refused.
            queue.deliver(successor, vec![0u8; 4]).expect("free");
            live.push(successor);
        }
        assert_eq!(live.len(), prepared, "every successor got in");
        assert_eq!(queue.len(), prepared, "the live bound held throughout");
        assert_eq!(
            queue.ready_len(),
            prepared,
            "and each successor is ready in its own right rather than on a stale marker"
        );
        // Every marker names a live flow, so a whole round services exactly the live set.
        let mut round = queue.begin_round();
        let mut serviced = 0usize;
        while queue.next(&mut round).is_some() {
            serviced += 1;
        }
        assert_eq!(serviced, live.len());
    }

    /// A new identity past the prepared bound is refused, and the refusal is about new rows only.
    ///
    /// The bound is the whole admission condition: a queue prepared for four holds four live identities, and
    /// the fifth is refused with the bound it was measured against in the refusal. What is *not* refused is
    /// anything to do with the four already here - re-admitting one of them takes no slot, and delivery and
    /// readiness go on working at a full queue, which is what keeps established flows serviced while newcomers
    /// are turned away.
    #[test]
    fn a_new_identity_past_the_bound_is_refused() {
        let mut queue: FairQueue<u32> = FairQueue::with_capacity(4);
        let mut worker = 0u64;
        let mut live = Vec::new();
        while queue.len() < queue.prepared() {
            worker += 1;
            let id = FlowId::new(live.len() as u32, worker);
            queue.admit(id).expect("inside the bound");
            live.push(id);
        }
        let full = queue.len();
        assert_eq!(full, 4, "the bound admits its whole count");
        let refused = FlowId::new(u32::MAX, worker + 1);
        assert_eq!(
            queue.admit(refused),
            Err(AtCapacity { prepared: 4 }),
            "one past the bound is refused rather than allocated"
        );
        assert_eq!(queue.len(), full, "and the refusal registered nothing");
        // The refusal is about *new* rows only: everything already here still works, including delivery and
        // readiness, and re-admitting a live identity is not a new row.
        let existing = live[0];
        queue.admit(existing).expect("already present");
        queue.deliver(existing, vec![7u8; 3]).expect("free");
        assert!(queue.owes(existing));
        assert_eq!(queue.ready_len(), 1);
        assert_eq!(queue.len(), full);
    }

    /// Retiring an identity takes its marker with it, so a handle reused by a successor starts clean rather
    /// than inheriting a queued wake.
    #[test]
    fn retirement_removes_the_stale_marker_before_the_handle_is_reused() {
        let mut queue = FairQueue::with_capacity(4);
        let stale = FlowId::new(1, 100);
        queue.admit(stale).expect("prepared");
        queue.deliver(stale, vec![1; 2]).expect("free");
        queue.mark_ready(stale);
        assert_eq!(queue.ready_len(), 1);
        queue.begin_retire(stale);
        assert_eq!(queue.ready_len(), 0, "the marker went with the discard");
        queue.finish_retire(stale);

        // The successor reusing the handle is a different identity with its own marker.
        let successor = FlowId::new(1, 101);
        queue.admit(successor).expect("prepared");
        assert_eq!(queue.ready_len(), 0);
        queue.mark_ready(successor);
        assert_eq!(queue.ready_len(), 1);
        let mut round = queue.begin_round();
        assert_eq!(queue.next(&mut round), Some(successor));
    }

    /// The charge is the row state plus the order, monotone in the bound and refused rather than wrapped.
    ///
    /// What it is *not* is the flow map's allocation: whatever that container keeps around its rows is
    /// count-bounded, so this figure is deliberately below what the map really takes. The order is a
    /// `VecDeque` and is covered in full, which is why it is the term compared against a real capacity here.
    #[test]
    fn the_charge_is_the_row_state_and_the_order() {
        for prepared in [0usize, 1, 3, 7, 8, 64, 129, 1_000] {
            let queue: FairQueue<u32> = FairQueue::with_capacity(prepared);
            let charged = FairQueue::<u32>::footprint(prepared).expect("a chargeable capacity");
            let rows =
                prepared as u64 * std::mem::size_of::<(FlowId<u32>, Pending<Vec<u8>>)>() as u64;
            let marker = std::mem::size_of::<FlowId<u32>>() as u64;
            let order = queue.order.capacity() as u64 * marker;
            assert!(
                charged >= rows + order + std::mem::size_of::<FairQueue<u32>>() as u64,
                "prepared {prepared}: charged {charged} does not cover its rows and its order"
            );
        }
        // Monotone, which is what a solver walking capacities depends on.
        assert!(FairQueue::<u32>::footprint(65) > FairQueue::<u32>::footprint(64));
        // A capacity whose accounting would wrap is not one that may be prepared.
        assert_eq!(FairQueue::<u32>::footprint(usize::MAX), None);
    }

    /// A stale payload, EOF or terminal cannot reach the flow that reused a handle, and the successor's own
    /// completion is unaffected by any of it.
    ///
    /// The half a handle-keyed design gets wrong is the second one: a stale marker deduplicated on the handle
    /// alone would suppress the successor's readiness, so the successor would sit with bytes nobody offered to
    /// send.
    #[test]
    fn a_stale_signal_cannot_retire_or_suppress_the_successor() {
        let mut queue = FairQueue::with_capacity(4);
        let stale = FlowId::new(1u32, 100);
        let successor = FlowId::new(1u32, 101);
        queue.admit(stale).expect("prepared");
        queue.deliver(stale, vec![1; 4]).expect("free");
        queue.signal_eof(stale);
        // Retired mid-flight, with its payload and its ordered EOF discarded before anything cancels it.
        assert_eq!(queue.begin_retire(stale), Some(vec![1; 4]));
        assert!(!queue.owes(stale));
        queue.finish_retire(stale);

        queue.admit(successor).expect("prepared");
        queue.deliver(successor, vec![9; 2]).expect("free");
        // Everything the predecessor could still emit lands on nothing.
        assert_eq!(
            queue.deliver(stale, vec![0]).expect_err("stale").1,
            Rejected::Unknown
        );
        queue.signal_eof(stale);
        queue.mark_ready(stale);
        assert_eq!(queue.serviced(stale, 2), Progress::Idle);
        // And none of it touched the successor: its payload is intact, its readiness is its own, and it
        // still owes exactly what it owed.
        assert!(queue.owes(successor));
        assert_eq!(queue.peek(successor), Some(&[9, 9][..]));
        let mut round = queue.begin_round();
        assert_eq!(queue.next(&mut round), Some(successor));
        assert_eq!(queue.next(&mut round), None);
        assert_eq!(queue.serviced(successor, 2), Progress::Consumed);
        assert!(!queue.owes(successor));
    }

    /// Cancellation may bypass a handover wait only after the owner has discarded exactly what that wait was
    /// for - and only that flow's.
    #[test]
    fn retirement_discards_this_identity_and_leaves_every_other_alone() {
        let mut queue = FairQueue::with_capacity(4);
        let c = FlowId::new(3u32, 300);
        for id in [a(), b(), c] {
            queue.admit(id).expect("prepared");
            queue.deliver(id, vec![0; 4]).expect("free");
            queue.signal_eof(id);
        }
        assert_eq!(queue.ready_len(), 3);

        // B retires: its payload comes back to the owner to drop, its EOF is gone, its marker is gone.
        assert_eq!(queue.begin_retire(b()), Some(vec![0; 4]));
        assert!(!queue.owes(b()));
        assert_eq!(queue.ready_len(), 2);
        // A and C still owe exactly what they owed, and still get their turns in order.
        assert!(queue.owes(a()));
        assert!(queue.owes(c));
        let serviced = drain(&mut queue, |_| 4);
        assert_eq!(serviced, vec![(a(), Progress::Eof), (c, Progress::Eof)]);
        queue.finish_retire(b());
        assert_eq!(queue.len(), 2);
    }

    /// One flow that never drains blocks neither the other's data, nor its half-close, nor its clean
    /// terminal - which is the whole of what "fair" has to mean here.
    #[test]
    fn a_stalled_flow_blocks_no_other_flows_data_half_close_or_terminal() {
        let mut queue = FairQueue::with_capacity(4);
        queue.admit(a()).expect("prepared");
        queue.admit(b()).expect("prepared");
        queue.deliver(a(), vec![0; 16]).expect("free");

        for round in 0..8 {
            queue.deliver(b(), vec![1; 4]).expect("B's row is free");
            if round == 7 {
                queue.signal_eof(b());
            }
            let serviced = drain(&mut queue, |id| if id == a() { 0 } else { usize::MAX });
            for (id, progress) in serviced {
                if id == a() {
                    assert_eq!(progress, Progress::Blocked, "A never drains");
                } else if round == 7 {
                    assert_eq!(progress, Progress::Eof, "B half-closes on time");
                } else {
                    assert_eq!(progress, Progress::Consumed);
                }
            }
        }
        // B owes nothing, so its worker may be joined and its grant released while A is still stuck; A owes
        // exactly what it owed and has lost nothing.
        assert!(!queue.owes(b()));
        assert!(queue.owes(a()));
        assert_eq!(queue.peek(a()).expect("intact").len(), 16);
    }

    /// The owner's discard is committed before anything cancels: after `begin_retire` the flow owes nothing,
    /// so a worker parked on a handover may be released - and not one instant before.
    ///
    /// The ordering this pins is the one a terminal gets wrong by default. Cancelling first releases a task
    /// while the owner still believes it owes that task's bytes; the owner then services a flow whose
    /// producer is gone, or worse, retires a flow whose payload the wire never took. Discarding first makes
    /// "nothing is owed" true before the wait is bypassed.
    #[test]
    fn nothing_is_owed_the_moment_the_owner_discards_and_not_before() {
        let mut queue = FairQueue::with_capacity(4);
        queue.admit(a()).expect("prepared");
        queue.admit(b()).expect("prepared");
        // A's row holds payload the wire has not taken and B's an ordered end of stream not yet delivered.
        queue.deliver(a(), vec![1; 4]).expect("free");
        queue.signal_eof(b());

        assert!(queue.owes(a()), "A's payload has not been taken");
        assert!(queue.owes(b()), "B's end of stream has not been delivered");
        // And A's row will take nothing else while that is true.
        assert!(!queue.accepts(a()));

        // A's owner commits the discard. From that instant A owes nothing and its worker may be released -
        // and B is entirely unaffected, still owing exactly what it owed.
        let discarded = queue.begin_retire(a()).expect("A had a chunk");
        assert_eq!(discarded, vec![1; 4]);
        assert!(!queue.owes(a()));
        assert!(queue.owes(b()), "B's wait is not bypassed by A's discard");
        // And nothing more may be admitted for the identity that is going.
        assert_eq!(
            queue.deliver(a(), vec![0]).expect_err("retiring").1,
            Rejected::Unknown
        );
        queue.mark_ready(a());
        let mut round = queue.begin_round();
        assert_eq!(queue.next(&mut round), Some(b()), "only B is serviceable");
        assert_eq!(queue.serviced(b(), 0), Progress::Eof);
        assert!(!queue.owes(b()));
    }

    /// A terminal for an identity the queue no longer holds does nothing at all - no discard, no readiness,
    /// no service - so it cannot reach the flow that reused its handle.
    #[test]
    fn a_stale_terminal_has_no_effect_on_the_successor() {
        let mut queue = FairQueue::with_capacity(4);
        let stale = FlowId::new(1u32, 100);
        let successor = FlowId::new(1u32, 101);
        queue.admit(stale).expect("prepared");
        queue.begin_retire(stale);
        queue.finish_retire(stale);
        queue.admit(successor).expect("prepared");
        queue.deliver(successor, vec![4; 4]).expect("free");
        queue.signal_eof(successor);

        // The predecessor's terminal arrives late. Every operation it could perform is a no-op.
        assert_eq!(queue.begin_retire(stale), None);
        queue.finish_retire(stale);
        assert_eq!(queue.serviced(stale, 4), Progress::Idle);
        assert!(!queue.owes(stale));

        // The successor still owes exactly what it owed, and still gets its turn.
        assert!(queue.owes(successor));
        assert_eq!(queue.peek(successor), Some(&[4, 4, 4, 4][..]));
        assert_eq!(queue.len(), 1);
        let mut round = queue.begin_round();
        assert_eq!(queue.next(&mut round), Some(successor));
        assert_eq!(queue.serviced(successor, 4), Progress::Eof);
    }

    /// Everything one admission builds, and what an unwind has to have given back.
    #[derive(Default)]
    struct Ledger {
        sockets: usize,
        channels: usize,
        leases: usize,
        tasks: usize,
        records: usize,
        next: u32,
    }

    /// An owner that records every side effect, so "before any side effect" is observed rather than argued.
    struct Recorder<'a> {
        ledger: &'a mut Ledger,
        room: usize,
        /// Refuse at `build`, at `admit`, or not at all - the three ways this can fail.
        refuse_build: bool,
        refuse_admit: bool,
    }

    /// The record: one socket, one channel pair and one grant. Which handle it belongs to is read back on
    /// the unwind path, so a record cannot be given back against the wrong one.
    struct Record {
        handle: u32,
    }

    impl FlowOps for Recorder<'_> {
        type Handle = u32;
        type Record = Record;
        type Error = &'static str;
        type Payload = Vec<u8>;

        fn has_room(&self) -> bool {
            self.ledger.records < self.room
        }

        fn build(&mut self) -> Result<(u32, u64, Record), &'static str> {
            if self.refuse_build {
                return Err("the socket could not be opened");
            }
            let handle = self.ledger.next;
            self.ledger.next += 1;
            self.ledger.sockets += 1;
            self.ledger.channels += 1;
            self.ledger.leases += 1;
            Ok((handle, u64::from(handle) + 1_000, Record { handle }))
        }

        fn unwind(&mut self, handle: u32, _worker: u64, record: Record) {
            assert_eq!(
                record.handle, handle,
                "a record may only be unwound against the handle it was built for"
            );
            self.ledger.sockets -= 1;
            self.ledger.channels -= 1;
            self.ledger.leases -= 1;
        }

        fn admit(&mut self, _handle: u32, _worker: u64, record: Record) -> Result<(), Record> {
            if self.refuse_admit {
                return Err(record);
            }
            self.ledger.records += 1;
            self.ledger.tasks += 1;
            Ok(())
        }
    }

    /// The same transaction, with every owner a production type: a real `Admission` lease, a real `Workers`
    /// table running a real gated task, a real smoltcp `SocketSet` holding a real TCP socket with real
    /// buffers, real Tokio channels for the flow's own queues, and this module's own
    /// `FairQueue`. Only the upstream open is injected, because that one needs an Android network.
    ///
    /// What this proves that a recorder cannot: the unwind really drops a smoltcp socket out of a real set,
    /// really cancels and joins a real task, really closes real channels, and really gives a real lease back.
    /// The transaction production runs: capacity in both tables before anything is built, then build, then
    /// register both collections, then admit - with every failure after the build giving everything back.
    #[test]
    fn a_flow_admission_leaves_nothing_behind_however_it_fails() {
        for prepared in [0usize, 1, 3, 16] {
            let mut fair: FairQueue<u32> = FairQueue::with_capacity(prepared);
            let mut order: VecDeque<u32> = VecDeque::with_capacity(prepared);
            let ready = fair.order.capacity();
            let outgoing = order.capacity();
            let mut ledger = Ledger::default();

            // The bound fills, and neither contiguous collection grows doing it. The flow map's own capacity
            // is not read: it is a documented lower bound on what still fits, so an equality across
            // insertions would be treating a number that cannot report an allocation as if it could. What
            // bounds that map is the gate every one of these admissions passed.
            for admitted in 0..prepared {
                let mut ops = Recorder {
                    ledger: &mut ledger,
                    room: prepared,
                    refuse_build: false,
                    refuse_admit: false,
                };
                admit_flow(&mut ops, &mut fair, &mut order, prepared)
                    .unwrap_or_else(|_| panic!("prepared {prepared}: flow {admitted} must fit"));
            }
            assert_eq!(ledger.records, prepared);
            assert_eq!(ledger.tasks, prepared);
            assert_eq!(fair.order.capacity(), ready, "prepared {prepared}");
            assert_eq!(order.capacity(), outgoing, "prepared {prepared}");

            // One past the bound is refused *before* any side effect: no socket, no channel, no grant, no
            // task. This is the check that has to come first, because a refusal after the socket is open is a
            // socket to close on the one path that only runs when the daemon is already at its limit.
            let before = (
                ledger.sockets,
                ledger.channels,
                ledger.leases,
                ledger.tasks,
                ledger.records,
            );
            let mut ops = Recorder {
                ledger: &mut ledger,
                room: prepared,
                refuse_build: false,
                refuse_admit: false,
            };
            assert_eq!(
                admit_flow(&mut ops, &mut fair, &mut order, prepared),
                Err(Refused::AtCapacity(AtCapacity { prepared }))
            );
            assert_eq!(
                (
                    ledger.sockets,
                    ledger.channels,
                    ledger.leases,
                    ledger.tasks,
                    ledger.records
                ),
                before,
                "prepared {prepared}: a refusal built nothing"
            );
            assert_eq!(fair.len(), prepared, "and registered nothing");
            assert_eq!(order.len(), prepared);
        }
    }

    /// A build that fails leaves nothing registered, and an admission that refuses gives back the socket,
    /// the channels, the grant and both registrations.
    #[test]
    fn a_refused_admission_unwinds_every_side_effect() {
        let prepared = 4usize;

        // The build itself failed - the socket would not open, or the grant was denied. It cleans up after
        // itself, so nothing is registered and nothing is left.
        let mut fair: FairQueue<u32> = FairQueue::with_capacity(prepared);
        let mut order: VecDeque<u32> = VecDeque::with_capacity(prepared);
        let mut ledger = Ledger::default();
        let mut ops = Recorder {
            ledger: &mut ledger,
            room: prepared,
            refuse_build: true,
            refuse_admit: false,
        };
        assert_eq!(
            admit_flow(&mut ops, &mut fair, &mut order, prepared),
            Err(Refused::Unbuildable("the socket could not be opened"))
        );
        assert!(fair.is_empty(), "nothing was registered");
        assert!(order.is_empty());
        assert_eq!(ledger.sockets, 0);
        assert_eq!(ledger.leases, 0);
        assert_eq!(ledger.tasks, 0);

        // The worker table refused after everything was built and both collections had taken it. The socket,
        // the channels, the grant *and* both registrations all go.
        let mut ops = Recorder {
            ledger: &mut ledger,
            room: prepared,
            refuse_build: false,
            refuse_admit: true,
        };
        let refused = admit_flow(&mut ops, &mut fair, &mut order, prepared);
        assert!(
            matches!(refused, Err(Refused::AtCapacity(_))),
            "{refused:?}"
        );
        assert_eq!(ledger.sockets, 0, "the socket went back");
        assert_eq!(ledger.channels, 0, "and the channels");
        assert_eq!(ledger.leases, 0, "and the grant");
        assert_eq!(ledger.tasks, 0, "and no task was left running");
        assert_eq!(ledger.records, 0);
        assert!(fair.is_empty(), "and the fair queue kept nothing");
        assert!(order.is_empty(), "nor the round-robin order");

        // And the owner still works afterwards: this refuses one flow, not the engine.
        let mut ops = Recorder {
            ledger: &mut ledger,
            room: prepared,
            refuse_build: false,
            refuse_admit: false,
        };
        assert!(admit_flow(&mut ops, &mut fair, &mut order, prepared).is_ok());
        assert_eq!(fair.len(), 1);
        assert_eq!(order.len(), 1);
    }

    /// The fair queue refusing *after* everything is built unwinds every resource, not just the registration.
    ///
    /// Reachable by giving the queue less capacity than the bound says: production's pre-check makes this
    /// unreachable, which is exactly why it has to be driven directly - the branch that only runs when an
    /// invariant has already broken is the one nobody exercises.
    #[test]
    fn a_fair_refusal_after_the_build_unwinds_the_socket_and_the_grant() {
        // The bound says four; the fair queue can hold one. The pre-check passes, the build happens, and the
        // registration is what refuses.
        let mut fair: FairQueue<u32> = FairQueue::with_capacity(1);
        let mut order: VecDeque<u32> = VecDeque::with_capacity(4);
        let mut ledger = Ledger::default();

        let mut ops = Recorder {
            ledger: &mut ledger,
            room: 4,
            refuse_build: false,
            refuse_admit: false,
        };
        admit_flow(&mut ops, &mut fair, &mut order, 4).expect("the first fits the queue");
        assert_eq!(ledger.sockets, 1);
        assert_eq!(ledger.tasks, 1);
        let held = (ledger.sockets, ledger.channels, ledger.leases, ledger.tasks);

        // The second passes the pre-check (the bound is four), builds its socket, its channels and its
        // grant, and is then refused by the queue.
        let mut ops = Recorder {
            ledger: &mut ledger,
            room: 4,
            refuse_build: false,
            refuse_admit: false,
        };
        let refused = admit_flow(&mut ops, &mut fair, &mut order, 4);
        assert_eq!(
            refused,
            Err(Refused::AtCapacity(AtCapacity { prepared: 1 })),
            "the fair queue's own capacity is what said no"
        );
        assert_eq!(
            (ledger.sockets, ledger.channels, ledger.leases, ledger.tasks),
            held,
            "and everything the refused flow built went back"
        );
        assert_eq!(fair.len(), 1, "the queue kept only what it had");
        assert_eq!(order.len(), 1, "and the order was not left holding it");

        // The first flow is untouched and still serviceable.
        let id = FlowId::new(0, 1_000);
        assert!(fair.is_admitted(id));
        fair.deliver(id, vec![1, 2, 3]).expect("free");
        let mut round = fair.begin_round();
        assert_eq!(fair.next(&mut round), Some(id));
    }

    /// An engine that can carry no flows builds nothing at all: no socket, no lease, no task, and neither
    /// collection allocating.
    #[test]
    fn a_zero_bound_admission_builds_nothing() {
        let mut fair: FairQueue<u32> = FairQueue::with_capacity(0);
        let mut order: VecDeque<u32> = VecDeque::with_capacity(0);
        let mut ledger = Ledger::default();
        let mut ops = Recorder {
            ledger: &mut ledger,
            room: 0,
            refuse_build: false,
            refuse_admit: false,
        };
        assert_eq!(
            admit_flow(&mut ops, &mut fair, &mut order, 0),
            Err(Refused::AtCapacity(AtCapacity { prepared: 0 }))
        );
        assert_eq!(ledger.sockets, 0);
        assert_eq!(ledger.leases, 0);
        assert_eq!(ledger.tasks, 0);
        assert_eq!(order.capacity(), 0, "the order allocated nothing");
    }

    /// The bound the tables were prepared for is the bound registration honours: exactly that many take
    /// without either collection growing, and the next one is refused with nothing half-done.
    #[test]
    fn registration_fills_the_prepared_bound_and_refuses_the_next_atomically() {
        for prepared in [0usize, 1, 4, 64] {
            let mut fair: FairQueue<u32> = FairQueue::with_capacity(prepared);
            let mut order: VecDeque<u32> = VecDeque::with_capacity(prepared);
            let ready = fair.order.capacity();
            let outgoing = order.capacity();

            for handle in 0..prepared as u32 {
                register(
                    &mut fair,
                    &mut order,
                    prepared,
                    FlowId::new(handle, u64::from(handle)),
                )
                .unwrap_or_else(|_| panic!("prepared {prepared}: handle {handle} must fit"));
            }
            assert_eq!(fair.len(), prepared);
            assert_eq!(order.len(), prepared);
            // Neither contiguous collection grew. The flow map is bounded by the gate each registration
            // passed rather than by a capacity reading, which could not report an allocation anyway.
            assert_eq!(fair.order.capacity(), ready, "prepared {prepared}");
            assert_eq!(order.capacity(), outgoing, "prepared {prepared}");

            // One past the bound is refused, and refused before either half took it - so there is nothing to
            // unwind and no residue in either collection.
            let extra = FlowId::new(prepared as u32, 9_999);
            assert_eq!(
                register(&mut fair, &mut order, prepared, extra),
                Err(AtCapacity { prepared })
            );
            assert_eq!(
                fair.len(),
                prepared,
                "prepared {prepared}: nothing admitted"
            );
            assert_eq!(order.len(), prepared, "prepared {prepared}: nothing queued");
            assert!(!fair.is_admitted(extra));
            assert_eq!(order.capacity(), outgoing);
        }
    }

    /// A refusal from the fair queue leaves the round-robin order untouched, and vice versa: there is no
    /// half-registered flow either way.
    #[test]
    fn a_refusal_from_either_half_leaves_no_residue() {
        // The fair queue refuses: its capacity is the smaller of the two, so it is the half that says no.
        let mut fair: FairQueue<u32> = FairQueue::with_capacity(1);
        let mut order: VecDeque<u32> = VecDeque::with_capacity(4);
        register(&mut fair, &mut order, 4, FlowId::new(1, 1)).expect("the first fits");
        let refused = register(&mut fair, &mut order, 4, FlowId::new(2, 2));
        assert_eq!(refused, Err(AtCapacity { prepared: 1 }));
        assert_eq!(fair.len(), 1, "the fair queue took nothing");
        assert_eq!(
            order.len(),
            1,
            "and the order was not left holding a flow the queue refused"
        );

        // The order refuses: it is checked first, so the fair queue is never asked.
        let mut fair: FairQueue<u32> = FairQueue::with_capacity(8);
        let mut order: VecDeque<u32> = VecDeque::with_capacity(1);
        register(&mut fair, &mut order, 1, FlowId::new(1, 1)).expect("the first fits");
        let refused = register(&mut fair, &mut order, 1, FlowId::new(2, 2));
        assert_eq!(refused, Err(AtCapacity { prepared: 1 }));
        assert_eq!(order.len(), 1);
        assert_eq!(fair.len(), 1, "the fair queue was never asked");
        assert!(!fair.is_admitted(FlowId::new(2, 2)));

        // And deregistering takes a flow out of both, handing its payload back for the owner to drop.
        fair.deliver(FlowId::new(1, 1), vec![7; 3]).expect("free");
        let discarded = deregister(&mut fair, &mut order, FlowId::new(1, 1));
        assert_eq!(discarded, Some(vec![7; 3]));
        assert!(fair.is_empty());
        assert!(order.is_empty());
    }

    /// An engine that can carry no flows registers none, and refuses the first without touching anything.
    #[test]
    fn a_zero_bound_registers_nothing() {
        let mut fair: FairQueue<u32> = FairQueue::with_capacity(0);
        let mut order: VecDeque<u32> = VecDeque::with_capacity(0);
        assert_eq!(
            register(&mut fair, &mut order, 0, FlowId::new(1, 1)),
            Err(AtCapacity { prepared: 0 })
        );
        assert!(fair.is_empty());
        assert!(order.is_empty());
        assert_eq!(order.capacity(), 0, "the order allocated nothing");
        // The fair queue's own minimum is what [FairQueue::footprint] charges for, and it is charged even at
        // zero - which is the point: a minimum quietly assumed free is a real allocation nobody charged.
        assert!(FairQueue::<u32>::footprint(0).expect("chargeable") > 0);
    }

    /// The order is explicit, and a flow that rotates to the back does not get a second turn in the same
    /// round - which is what stops one flow's retries from being the round.
    #[test]
    fn the_round_budget_is_taken_when_the_round_begins() {
        let mut queue = FairQueue::with_capacity(8);
        let c = FlowId::new(3, 300);
        for id in [a(), b(), c] {
            queue.admit(id).expect("prepared");
            queue.deliver(id, vec![0; 4]).expect("free");
        }
        let serviced = drain(&mut queue, |_| 1);
        // Each of the three exactly once, in the order they became ready, none of them twice.
        assert_eq!(
            serviced,
            vec![
                (a(), Progress::Blocked),
                (b(), Progress::Blocked),
                (c, Progress::Blocked)
            ]
        );
        // And all three are queued again for the next round, still in order.
        assert_eq!(queue.ready_len(), 3);
        let serviced = drain(&mut queue, |_| 3);
        assert_eq!(
            serviced,
            vec![
                (a(), Progress::Consumed),
                (b(), Progress::Consumed),
                (c, Progress::Consumed)
            ]
        );
    }
}
