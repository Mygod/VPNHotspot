//! Which flow a signal is actually about, whose turn it is, and the transaction that admits one.
//!
//! # Identity, because handles are reused
//!
//! smoltcp hands back socket handles, so a handle alone names a slot rather than a flow: a terminal from a
//! closed flow and a request belonging to the flow that reused its handle are indistinguishable by handle.
//! Every owner-side operation therefore takes the retained worker identity beside the handle and validates
//! the pair. A stale identity is discarded on sight and - the part that matters - cannot suppress the
//! successor that took its slot.
//!
//! # Admission is a transaction
//!
//! A flow is a socket in a real set, a charged grant, a bounded byte bridge and a running task, taken in an
//! order where each step can undo the ones before it. [admit_flow] owns that order so it is production's
//! rather than each caller's.
//!
//! # Turns are the fairness
//!
//! [Turns] is the one place a flow's place in the round-robin lives: admission puts it there, a pass takes
//! every flow out of it exactly once, a refused candidate takes back exactly what it put in, and a reclaimed
//! flow takes its own place with it. There is deliberately no second path - no bare deque beside it and no
//! method that pushes or pops without saying which of those four things it is - because both bugs this type
//! exists to prevent were invisible at the call site: a pass that rotated by popping and pushing every entry
//! restored the order it started from, and a rollback that scanned for the candidate's handle deregistered a
//! *predecessor* holding the same one.
//!
//! Nothing here is async, and nothing here moves a byte. What one flow's bytes cross on is an ordinary
//! bounded Tokio stream - see [crate::shared::bridge] - whose readiness and backpressure are the library's
//! own, so there is no payload to hold and no readiness marker to deduplicate.

use std::collections::VecDeque;
use std::net::SocketAddr;

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

/// Admitting a flow would exceed the fixed bound the owner's tables were charged for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtCapacity {
    pub prepared: usize,
}

/// The order one owner serves its flows in, and the rotation that keeps a pass from always starting at the
/// same one.
///
/// Explicit rather than a walk of the owner's map: a `HashMap`'s iteration order is arbitrary and changes
/// when it is resized, which is not fairness but a different unfairness each time.
///
/// Bounded by the `prepared` the owner's tables were charged for. Nothing here grows it: [admit_flow] refuses
/// before it builds anything, and every other operation removes or reads.
pub struct Turns<H> {
    order: VecDeque<H>,
    /// How many turns of the pass that is running have been handed out. Zero between passes, which is every
    /// moment an owner admits or reclaims a flow - a pass is one synchronous walk and nothing else happens
    /// inside it.
    served: usize,
}

impl<H: Copy + Eq> Turns<H> {
    /// Prepares for `prepared` live flows, so the common case allocates nothing. That is an initial
    /// reservation rather than the bound: what enforces the bound is [admit_flow]'s refusal.
    pub fn with_capacity(prepared: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(prepared),
            served: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// What the backing really reserved, which is what a charge is checked against - never a bound anything
    /// admits on.
    pub fn capacity(&self) -> usize {
        self.order.capacity()
    }

    /// Every live flow, in the order they are served, for an owner that only needs to *find* them - a
    /// retirement, an expiry sweep, a walk for sockets the stack has finished with.
    ///
    /// Read-only, and it moves no cursor: a walk that also took turns would rotate the order behind whichever
    /// pass was running.
    pub fn iter(&self) -> impl Iterator<Item = &H> {
        self.order.iter()
    }

    /// The next flow's turn in the pass that is running, or `None` once every live flow has had exactly one -
    /// which is also what ends the pass and moves the starting position on.
    ///
    /// One call rather than an index and a separate rotate, because the rotation *is* the fairness and a
    /// caller that forgot it would still look like a round robin: every pass would simply start at the same
    /// flow again, and the flow at the front would be first to the client's send buffer for ever. Ending the
    /// pass is the only thing that rotates, so a pass abandoned part-way leaves the order where it was.
    pub fn turn(&mut self) -> Option<H> {
        match self.order.get(self.served) {
            Some(handle) => {
                self.served += 1;
                Some(*handle)
            }
            None => {
                self.served = 0;
                // The flow that went first this pass goes last in the next one.
                if !self.order.is_empty() {
                    self.order.rotate_left(1);
                }
                None
            }
        }
    }

    /// Gives one flow the last place in the order.
    ///
    /// Infallible, and the bound is [admit_flow]'s: it refuses before it builds anything and the owner is
    /// synchronous from that refusal to here, so there is no second moment at which this order could be full.
    /// A capacity check here would be a branch nothing can reach and a test nothing can honestly write.
    fn admit(&mut self, handle: H) {
        self.order.push_back(handle);
    }

    /// Takes back exactly the place [Turns::admit] just gave out, and nothing else.
    ///
    /// `pop_back` against the handle rather than a scan for it. The append is the last thing that happened to
    /// this order - the owner is synchronous from the build to the refusal - so the entry to take back is the
    /// one at the back, and its position is known rather than searched for. A scan would match a
    /// *predecessor* holding the same handle and deregister a live flow that is still being served, which is
    /// a client whose bytes stop moving for no reason it or anyone else can see.
    fn undo(&mut self, handle: H) {
        if self.order.back() == Some(&handle) {
            self.order.pop_back();
        }
    }

    /// Forgets a flow that is being reclaimed, taking the one place it holds.
    ///
    /// One place rather than every place that names it: a flow leaving takes its own turn with it and nothing
    /// else's, which is the same exactness [Turns::undo] keeps for the same reason.
    pub fn forget(&mut self, handle: H) {
        if let Some(index) = self.order.iter().position(|queued| *queued == handle) {
            self.order.remove(index);
        }
    }
}

/// Which live flow a segment names, by the endpoint pair that identifies it.
///
/// Here rather than written out at each call site because both callers are decisions a wrong match makes
/// *silently* wrong rather than broken: one decides whether a `SYN` opens a second flow for a connection that
/// already has one, and the other decides which flow a client's reset aborts. Matching too loosely aborts a
/// stranger's connection; matching on one endpoint aborts every flow to the same server. Neither shows up as
/// a failure anywhere near where it was caused.
///
/// Both endpoints, and both in the direction the client's own segments carry them: `client` is the TUN-visible
/// source - Android's inner NAT address on IPv4, which is why it is never treated as an identity on its own -
/// and `destination` is where the flow is going. A segment travelling the other way is not this lookup's
/// input and would find nothing.
pub fn named_by<H>(
    flows: impl IntoIterator<Item = (H, SocketAddr, SocketAddr)>,
    client: SocketAddr,
    destination: SocketAddr,
) -> Option<H> {
    flows
        .into_iter()
        .find(|(_, from, to)| *from == client && *to == destination)
        .map(|(handle, _, _)| handle)
}

/// Everything one flow admission needs to build, and everything it needs to undo.
///
/// A trait rather than a closure so the *sequence* below is production's rather than each caller's: the
/// engine's implementation opens a smoltcp socket, takes a charged grant, builds the flow's byte bridge, and
/// hands the record to the worker table. Which of those exists at each step is the property under test, and a
/// caller that inlined the sequence would be a second copy of it.
pub trait FlowOps {
    /// Names one flow's transport slot. smoltcp's `SocketHandle` in production.
    type Handle: Copy + Eq;
    /// The record the worker table takes, and what an unwind has to give back.
    type Record;
    /// Why a build failed, which the caller reports its own way.
    type Error;

    /// Whether the worker table can take another record without growing. Asked *before* anything is built,
    /// so a refusal costs nothing to unwind.
    fn has_room(&self) -> bool;

    /// Opens the socket, takes the grant, builds the bridge, and answers the identity they belong to.
    fn build(&mut self) -> Result<(Self::Handle, u64, Self::Record), Self::Error>;

    /// Undoes [FlowOps::build] exactly: the socket, the bridge and the grant all go.
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
    /// The socket, the grant or the bridge could not be made.
    Unbuildable(E),
}

/// Admits one flow, or leaves nothing behind.
///
/// The order is the whole of it, and every step of it was wrong at some point:
///
/// 1. **capacity first**, in *both* tables, before a descriptor is opened or a byte is charged - a refusal
///    that arrives after the socket is open is a socket to close and a grant to release, on a path that only
///    runs when the daemon is already at its limit;
/// 2. **build** the socket, the grant and the bridge;
/// 3. **register** a turn, which is what gives this flow a place in every pass the owner makes over its
///    table - a flow that has none is one whose bytes nothing would ever move;
/// 4. **admit** to the worker table, which starts the task.
///
/// A failure at step 4 gives back exactly what steps 2 and 3 took, in reverse. There is deliberately no
/// second capacity check between 2 and 3: the owner is synchronous across all four, so nothing can enter
/// either table in between, and a branch that cannot be reached is one no test can honestly cover.
pub fn admit_flow<O: FlowOps>(
    ops: &mut O,
    turns: &mut Turns<O::Handle>,
    prepared: usize,
) -> Result<O::Handle, Refused<O::Error>> {
    // Both tables, before anything exists to unwind.
    if !ops.has_room() || turns.len() >= prepared {
        return Err(Refused::AtCapacity(AtCapacity { prepared }));
    }
    let (handle, worker, record) = ops.build().map_err(Refused::Unbuildable)?;
    turns.admit(handle);
    if let Err(record) = ops.admit(handle, worker, record) {
        // Exactly the place just taken, never every place this handle holds - see [Turns::undo].
        turns.undo(handle);
        ops.unwind(handle, worker, record);
        return Err(Refused::AtCapacity(AtCapacity { prepared }));
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {

    /// Two flows to the same server from different clients, and one client with two flows.
    fn wired() -> Vec<(u8, SocketAddr, SocketAddr)> {
        let server: SocketAddr = "198.51.100.7:443".parse().unwrap();
        let other: SocketAddr = "198.51.100.9:443".parse().unwrap();
        vec![
            (1, "192.0.2.1:40000".parse().unwrap(), server),
            (2, "192.0.2.2:40000".parse().unwrap(), server),
            (3, "192.0.2.1:40001".parse().unwrap(), other),
        ]
    }

    #[test]
    fn a_segment_names_the_flow_with_both_of_its_endpoints_and_no_other() {
        let flows = wired();
        assert_eq!(
            named_by(
                flows.clone(),
                "192.0.2.2:40000".parse().unwrap(),
                "198.51.100.7:443".parse().unwrap()
            ),
            Some(2),
            "the exact pair, not the first flow to that server"
        );
        // Same client, different destination - and the same destination, different client. Either half alone
        // would abort a connection that is not this one.
        assert_eq!(
            named_by(
                flows.clone(),
                "192.0.2.1:40000".parse().unwrap(),
                "198.51.100.9:443".parse().unwrap()
            ),
            None
        );
        assert_eq!(
            named_by(
                flows.clone(),
                "192.0.2.3:40000".parse().unwrap(),
                "198.51.100.7:443".parse().unwrap()
            ),
            None
        );
        // A port is part of both endpoints.
        assert_eq!(
            named_by(
                flows.clone(),
                "192.0.2.1:40001".parse().unwrap(),
                "198.51.100.9:443".parse().unwrap()
            ),
            Some(3)
        );
        // And the reversed pair is a different lookup, not the same flow seen from the other side.
        assert_eq!(
            named_by(
                flows,
                "198.51.100.7:443".parse().unwrap(),
                "192.0.2.1:40000".parse().unwrap()
            ),
            None
        );
    }

    #[test]
    fn nothing_is_named_when_there_is_nothing_to_name() {
        assert_eq!(
            named_by(
                Vec::<(u8, SocketAddr, SocketAddr)>::new(),
                "192.0.2.1:40000".parse().unwrap(),
                "198.51.100.7:443".parse().unwrap()
            ),
            None
        );
    }
    use super::*;

    /// Everything one admission builds, and what an unwind has to have given back.
    #[derive(Default)]
    struct Ledger {
        sockets: usize,
        bridges: usize,
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
        /// The handle `build` hands back, when the test needs a particular one. `None` takes the next free
        /// number, which is what a socket set does.
        handle: Option<u32>,
    }

    impl Recorder<'_> {
        fn new(ledger: &mut Ledger, room: usize) -> Recorder<'_> {
            Recorder {
                ledger,
                room,
                refuse_build: false,
                refuse_admit: false,
                handle: None,
            }
        }
    }

    /// The record: one socket, one bridge and one grant. Which handle it belongs to is read back on the
    /// unwind path, so a record cannot be given back against the wrong one.
    struct Record {
        handle: u32,
    }

    impl FlowOps for Recorder<'_> {
        type Handle = u32;
        type Record = Record;
        type Error = &'static str;

        fn has_room(&self) -> bool {
            self.ledger.records < self.room
        }

        fn build(&mut self) -> Result<(u32, u64, Record), &'static str> {
            if self.refuse_build {
                return Err("the socket could not be opened");
            }
            let handle = self.handle.unwrap_or(self.ledger.next);
            self.ledger.next = self.ledger.next.max(handle) + 1;
            self.ledger.sockets += 1;
            self.ledger.bridges += 1;
            self.ledger.leases += 1;
            Ok((handle, u64::from(handle) + 1_000, Record { handle }))
        }

        fn unwind(&mut self, handle: u32, _worker: u64, record: Record) {
            assert_eq!(
                record.handle, handle,
                "a record may only be unwound against the handle it was built for"
            );
            self.ledger.sockets -= 1;
            self.ledger.bridges -= 1;
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

    /// The order as it stands, without taking a turn - so a test can read it between passes without moving
    /// the very cursor it is checking.
    fn order(turns: &Turns<u32>) -> Vec<u32> {
        turns.iter().copied().collect()
    }

    /// One whole pass, answering which flows were served and in what order.
    fn pass(turns: &mut Turns<u32>) -> Vec<u32> {
        let mut served = Vec::new();
        while let Some(handle) = turns.turn() {
            served.push(handle);
        }
        served
    }

    #[test]
    fn a_pass_serves_every_flow_once_and_the_next_pass_starts_further_on() {
        let mut turns = Turns::with_capacity(3);
        for handle in [7u32, 8, 9] {
            turns.admit(handle);
        }
        // Every flow exactly once, and in the order they hold.
        assert_eq!(pass(&mut turns), vec![7, 8, 9]);
        // Ending the pass is what moves the starting position on. A pass that popped and pushed every entry
        // instead would restore the order it started from and serve 7 first for ever - which still reads like
        // a round robin at the call site, and is the bug this asserts against.
        assert_eq!(order(&turns), vec![8, 9, 7]);
        assert_eq!(pass(&mut turns), vec![8, 9, 7]);
        assert_eq!(order(&turns), vec![9, 7, 8]);
        assert_eq!(pass(&mut turns), vec![9, 7, 8]);
        // Three flows, three passes, and every one of them has been first exactly once.
        assert_eq!(order(&turns), vec![7, 8, 9]);
    }

    #[test]
    fn an_abandoned_pass_leaves_the_order_where_it_was() {
        let mut turns = Turns::with_capacity(3);
        for handle in [7u32, 8, 9] {
            turns.admit(handle);
        }
        // Only ending a pass rotates, so a caller that stopped part-way cannot hand the front of the order to
        // whichever flow it happened to stop at.
        assert_eq!(turns.turn(), Some(7));
        assert_eq!(order(&turns), vec![7, 8, 9]);
    }

    #[test]
    fn an_empty_order_has_no_turns_and_nothing_to_rotate() {
        let mut turns: Turns<u32> = Turns::with_capacity(0);
        assert!(turns.is_empty());
        assert_eq!(pass(&mut turns), Vec::<u32>::new());
        assert_eq!(turns.capacity(), 0, "and it allocated nothing");
    }

    #[test]
    fn a_reclaimed_flow_takes_one_place_and_the_rest_keep_theirs() {
        let mut turns = Turns::with_capacity(3);
        for handle in [7u32, 8, 9] {
            turns.admit(handle);
        }
        turns.forget(8);
        assert_eq!(order(&turns), vec![7, 9]);
        assert_eq!(pass(&mut turns), vec![7, 9]);
        // Forgetting one nobody holds is not an error and takes nothing.
        turns.forget(8);
        assert_eq!(order(&turns), vec![9, 7]);
    }

    #[test]
    fn a_refused_duplicate_takes_back_only_its_own_place_in_the_order() {
        let prepared = 4usize;
        let mut turns = Turns::with_capacity(prepared);
        // A live predecessor, already being served under handle 7. In production a socket set cannot hand out
        // a handle a live flow holds; this is what happens if it ever does, and the answer has to be that the
        // live flow is untouched rather than silently unscheduled.
        turns.admit(7);
        let mut ledger = Ledger::default();
        let mut ops = Recorder {
            refuse_admit: true,
            handle: Some(7),
            ..Recorder::new(&mut ledger, prepared)
        };
        assert!(matches!(
            admit_flow(&mut ops, &mut turns, prepared),
            Err(Refused::AtCapacity(_))
        ));
        // The predecessor still has its turn. A rollback that scanned for the handle would have taken both
        // entries and left a live flow in no pass at all.
        assert_eq!(order(&turns), vec![7]);
        assert_eq!(pass(&mut turns), vec![7]);
        // And exactly the candidate's own resources went back.
        assert_eq!(ledger.sockets, 0);
        assert_eq!(ledger.bridges, 0);
        assert_eq!(ledger.leases, 0);
        assert_eq!(ledger.tasks, 0);
        assert_eq!(ledger.records, 0);
    }

    #[test]
    fn a_flow_admission_leaves_nothing_behind_however_it_fails() {
        for prepared in [0usize, 1, 3, 16] {
            let mut turns: Turns<u32> = Turns::with_capacity(prepared);
            let reserved = turns.capacity();
            let mut ledger = Ledger::default();

            // The bound fills, and the order does not grow doing it.
            for admitted in 0..prepared {
                let mut ops = Recorder::new(&mut ledger, prepared);
                admit_flow(&mut ops, &mut turns, prepared)
                    .unwrap_or_else(|_| panic!("prepared {prepared}: flow {admitted} must fit"));
            }
            assert_eq!(ledger.records, prepared);
            assert_eq!(ledger.tasks, prepared);
            assert_eq!(turns.capacity(), reserved, "prepared {prepared}");

            // One past the bound is refused *before* any side effect: no socket, no bridge, no grant, no
            // task. This is the check that has to come first, because a refusal after the socket is open is a
            // socket to close on the one path that only runs when the daemon is already at its limit.
            let before = (
                ledger.sockets,
                ledger.bridges,
                ledger.leases,
                ledger.tasks,
                ledger.records,
            );
            let mut ops = Recorder::new(&mut ledger, prepared);
            assert_eq!(
                admit_flow(&mut ops, &mut turns, prepared),
                Err(Refused::AtCapacity(AtCapacity { prepared }))
            );
            assert_eq!(
                (
                    ledger.sockets,
                    ledger.bridges,
                    ledger.leases,
                    ledger.tasks,
                    ledger.records
                ),
                before,
                "prepared {prepared}: a refusal built nothing"
            );
            assert_eq!(turns.len(), prepared, "and registered nothing");
        }
    }

    /// A build that fails leaves nothing registered, and an admission that refuses gives back the socket,
    /// the bridge, the grant and the turn.
    #[test]
    fn a_refused_admission_unwinds_every_side_effect() {
        let prepared = 4usize;

        // The build itself failed - the socket would not open, or the grant was denied. It cleans up after
        // itself, so nothing is registered and nothing is left.
        let mut turns: Turns<u32> = Turns::with_capacity(prepared);
        let mut ledger = Ledger::default();
        let mut ops = Recorder {
            refuse_build: true,
            ..Recorder::new(&mut ledger, prepared)
        };
        assert_eq!(
            admit_flow(&mut ops, &mut turns, prepared),
            Err(Refused::Unbuildable("the socket could not be opened"))
        );
        assert!(turns.is_empty(), "nothing was registered");
        assert_eq!(ledger.sockets, 0);
        assert_eq!(ledger.leases, 0);
        assert_eq!(ledger.tasks, 0);

        // The worker table refused after everything was built and the order had taken a turn for it. The
        // socket, the bridge, the grant *and* the turn all go.
        let mut ops = Recorder {
            refuse_admit: true,
            ..Recorder::new(&mut ledger, prepared)
        };
        let refused = admit_flow(&mut ops, &mut turns, prepared);
        assert!(
            matches!(refused, Err(Refused::AtCapacity(_))),
            "{refused:?}"
        );
        assert_eq!(ledger.sockets, 0, "the socket went back");
        assert_eq!(ledger.bridges, 0, "and the bridge");
        assert_eq!(ledger.leases, 0, "and the grant");
        assert_eq!(ledger.tasks, 0, "and no task was left running");
        assert_eq!(ledger.records, 0);
        assert!(turns.is_empty(), "and the order kept no turn for it");

        // And the owner still works afterwards: this refuses one flow, not the engine.
        let mut ops = Recorder::new(&mut ledger, prepared);
        assert!(admit_flow(&mut ops, &mut turns, prepared).is_ok());
        assert_eq!(turns.len(), 1);
    }

    /// An engine that can carry no flows builds nothing at all: no socket, no lease, no task, and the order
    /// allocating nothing.
    #[test]
    fn a_zero_bound_admission_builds_nothing() {
        let mut turns: Turns<u32> = Turns::with_capacity(0);
        let mut ledger = Ledger::default();
        let mut ops = Recorder::new(&mut ledger, 0);
        assert_eq!(
            admit_flow(&mut ops, &mut turns, 0),
            Err(Refused::AtCapacity(AtCapacity { prepared: 0 }))
        );
        assert_eq!(ledger.sockets, 0);
        assert_eq!(ledger.leases, 0);
        assert_eq!(ledger.tasks, 0);
        assert_eq!(turns.capacity(), 0, "the order allocated nothing");
    }
}
