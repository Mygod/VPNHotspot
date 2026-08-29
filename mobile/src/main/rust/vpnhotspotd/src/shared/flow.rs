//! Flow identities pair reusable socket handles with monotonically assigned incarnations.
use std::collections::VecDeque;
use std::net::SocketAddr;

/// One flow, named by the pair that actually identifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FlowId<H> {
    pub handle: H,
    pub incarnation: u64,
}

impl<H> FlowId<H> {
    pub fn new(handle: H, incarnation: u64) -> Self {
        Self {
            handle,
            incarnation,
        }
    }
}

/// Admitting a flow would exceed the fixed bound the owner's tables were charged for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtCapacity {
    pub prepared: usize,
}

/// The order one owner serves its flows in, and the rotation that keeps a pass from always starting at the
/// same one.
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
    pub fn iter(&self) -> impl Iterator<Item = &H> {
        self.order.iter()
    }

    /// The next flow's turn in the pass that is running, or `None` once every live flow has had exactly one -
    /// which is also what ends the pass and moves the starting position on.
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
    fn admit(&mut self, handle: H) {
        self.order.push_back(handle);
    }

    /// Takes back exactly the place [Turns::admit] just gave out, and nothing else.
    fn undo(&mut self, handle: H) {
        if self.order.back() == Some(&handle) {
            self.order.pop_back();
        }
    }

    /// Forgets a flow that is being reclaimed, taking the one place it holds.
    pub fn forget(&mut self, handle: H) {
        if let Some(index) = self.order.iter().position(|queued| *queued == handle) {
            self.order.remove(index);
        }
    }
}

/// Which live flow a segment names, by the endpoint pair that identifies it.
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

    /// Opens the socket, takes the grant, builds the bridge, and answers the incarnation they belong to.
    fn build(&mut self) -> Result<(Self::Handle, u64, Self::Record), Self::Error>;

    /// Undoes [FlowOps::build] exactly: the socket, the bridge and the grant all go.
    fn unwind(&mut self, handle: Self::Handle, incarnation: u64, record: Self::Record);

    /// Hands the record to the worker table and starts its task. The record comes back on refusal.
    fn admit(
        &mut self,
        handle: Self::Handle,
        incarnation: u64,
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
pub fn admit_flow<O: FlowOps>(
    ops: &mut O,
    turns: &mut Turns<O::Handle>,
    prepared: usize,
) -> Result<O::Handle, Refused<O::Error>> {
    // Both tables, before anything exists to unwind.
    if !ops.has_room() || turns.len() >= prepared {
        return Err(Refused::AtCapacity(AtCapacity { prepared }));
    }
    let (handle, incarnation, record) = ops.build().map_err(Refused::Unbuildable)?;
    turns.admit(handle);
    if let Err(record) = ops.admit(handle, incarnation, record) {
        // Exactly the place just taken, never every place this handle holds - see [Turns::undo].
        turns.undo(handle);
        ops.unwind(handle, incarnation, record);
        return Err(Refused::AtCapacity(AtCapacity { prepared }));
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {

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
        assert_eq!(
            named_by(
                flows.clone(),
                "192.0.2.1:40001".parse().unwrap(),
                "198.51.100.9:443".parse().unwrap()
            ),
            Some(3)
        );
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

    #[derive(Default)]
    struct Ledger {
        sockets: usize,
        bridges: usize,
        leases: usize,
        tasks: usize,
        records: usize,
        next: u32,
    }

    struct Recorder<'a> {
        ledger: &'a mut Ledger,
        room: usize,
        refuse_build: bool,
        refuse_admit: bool,
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

        fn unwind(&mut self, handle: u32, _incarnation: u64, record: Record) {
            assert_eq!(
                record.handle, handle,
                "a record may only be unwound against the handle it was built for"
            );
            self.ledger.sockets -= 1;
            self.ledger.bridges -= 1;
            self.ledger.leases -= 1;
        }

        fn admit(&mut self, _handle: u32, _incarnation: u64, record: Record) -> Result<(), Record> {
            if self.refuse_admit {
                return Err(record);
            }
            self.ledger.records += 1;
            self.ledger.tasks += 1;
            Ok(())
        }
    }

    fn order(turns: &Turns<u32>) -> Vec<u32> {
        turns.iter().copied().collect()
    }

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
        assert_eq!(pass(&mut turns), vec![7, 8, 9]);
        assert_eq!(order(&turns), vec![8, 9, 7]);
        assert_eq!(pass(&mut turns), vec![8, 9, 7]);
        assert_eq!(order(&turns), vec![9, 7, 8]);
        assert_eq!(pass(&mut turns), vec![9, 7, 8]);
        assert_eq!(order(&turns), vec![7, 8, 9]);
    }

    #[test]
    fn an_abandoned_pass_leaves_the_order_where_it_was() {
        let mut turns = Turns::with_capacity(3);
        for handle in [7u32, 8, 9] {
            turns.admit(handle);
        }
        assert_eq!(turns.turn(), Some(7));
        assert_eq!(order(&turns), vec![7, 8, 9]);
    }

    #[test]
    fn an_empty_order_has_no_turns_and_nothing_to_rotate() {
        let mut turns: Turns<u32> = Turns::with_capacity(0);
        assert!(turns.is_empty());
        assert_eq!(pass(&mut turns), Vec::<u32>::new());
        assert_eq!(turns.capacity(), 0);
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
        turns.forget(8);
        assert_eq!(order(&turns), vec![9, 7]);
    }

    #[test]
    fn a_refused_duplicate_takes_back_only_its_own_place_in_the_order() {
        let prepared = 4usize;
        let mut turns = Turns::with_capacity(prepared);
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
        assert_eq!(order(&turns), vec![7]);
        assert_eq!(pass(&mut turns), vec![7]);
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

            for admitted in 0..prepared {
                let mut ops = Recorder::new(&mut ledger, prepared);
                admit_flow(&mut ops, &mut turns, prepared)
                    .unwrap_or_else(|_| panic!("prepared {prepared}: flow {admitted} must fit"));
            }
            assert_eq!(ledger.records, prepared);
            assert_eq!(ledger.tasks, prepared);
            assert_eq!(turns.capacity(), reserved, "prepared {prepared}");

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
            assert_eq!(turns.len(), prepared);
        }
    }

    #[test]
    fn a_refused_admission_unwinds_every_side_effect() {
        let prepared = 4usize;

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
        assert!(turns.is_empty());
        assert_eq!(ledger.sockets, 0);
        assert_eq!(ledger.leases, 0);
        assert_eq!(ledger.tasks, 0);

        let mut ops = Recorder {
            refuse_admit: true,
            ..Recorder::new(&mut ledger, prepared)
        };
        let refused = admit_flow(&mut ops, &mut turns, prepared);
        assert!(
            matches!(refused, Err(Refused::AtCapacity(_))),
            "{refused:?}"
        );
        assert_eq!(ledger.sockets, 0);
        assert_eq!(ledger.bridges, 0);
        assert_eq!(ledger.leases, 0);
        assert_eq!(ledger.tasks, 0);
        assert_eq!(ledger.records, 0);
        assert!(turns.is_empty());

        let mut ops = Recorder::new(&mut ledger, prepared);
        assert!(admit_flow(&mut ops, &mut turns, prepared).is_ok());
        assert_eq!(turns.len(), 1);
    }

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
        assert_eq!(turns.capacity(), 0);
    }
}
