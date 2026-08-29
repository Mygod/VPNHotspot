use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crate::shared::admission::logical_footprint;

/// How long a receiver may hold fragments of one datagram, and therefore how long an Identification stays
/// dangerous after it reaches the wire.
pub const NONREUSE_WINDOW: Duration = Duration::from_secs(60);

/// How many Identifications one tuple's sequence has before it is spent.
const CYCLE: u32 = 1 << 16;

/// What a receiver reassembles on, minus the Identification this hands out.
pub type Tuple = (Ipv4Addr, Ipv4Addr, u8);

/// One guarded datagram's identity, issued before it is built and carried by every packet it becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guarded {
    tuple: Tuple,
    identification: u16,
}

impl Guarded {
    /// The value the header carries, which is the only part a producer has any use for.
    pub fn identification(&self) -> u16 {
        self.identification
    }

    pub fn tuple(&self) -> Tuple {
        self.tuple
    }
}

/// What the TUN writer says about one guarded packet it took ownership of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terminal {
    guarded: Guarded,
    /// The moment the write succeeded, taken after it did. `None` when the packet never reached the wire.
    written: Option<Instant>,
}

impl Terminal {
    /// The packet reached the wire at `at`, which the writer reads *after* the write returns rather than
    /// before it starts.
    pub fn wrote(guarded: Guarded, at: Instant) -> Self {
        Self {
            guarded,
            written: Some(at),
        }
    }

    /// The writer owned the packet and it never reached the wire.
    pub fn unwritten(guarded: Guarded) -> Self {
        Self {
            guarded,
            written: None,
        }
    }

    pub fn guarded(&self) -> Guarded {
        self.guarded
    }

    pub fn written(&self) -> Option<Instant> {
        self.written
    }
}

/// Why one guarded datagram could not be given an Identification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// This session has not been open for [NONREUSE_WINDOW] yet, so a value it issued could still collide
    /// with one its predecessor wrote.
    Quarantined,
    /// The table holds its prepared capacity and this tuple is not in it.
    AtCapacity,
    /// This tuple has issued all 65,536 of its Identifications and may not start again yet.
    Exhausted,
}

/// Answered when a guarded packet may not be handed to the writer because its ending could not be tracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Untracked;

/// What one session's allocator is prepared for.
#[derive(Debug, Clone, Copy)]
pub struct Prepared {
    pub tuples: usize,
    pub tracked: usize,
    pub opened: Instant,
}

/// One tuple's whole state. Bounded, and deliberately so: a per-Identification history would be 8 KiB per
/// tuple to permit an out-of-order reuse a sequence never needs.
struct Entry {
    /// How many of this cycle's Identifications have been issued. [CYCLE] means the cycle is spent.
    issued: u32,
    /// Packets of this tuple the writer has accepted and not yet settled. Reuse waits for this to be zero,
    /// because a packet still in the queue is one whose wire time is not yet known.
    pending: u32,
    /// The latest moment a packet of this tuple reached the wire. `None` when none ever has, which is a tuple
    /// with nothing out there to collide with.
    written: Option<Instant>,
}

impl Entry {
    /// Whether this tuple's sequence may start again, or its bucket be given to someone else.
    fn reusable(&self, now: Instant) -> bool {
        self.pending == 0
            && self
                .written
                .is_none_or(|at| now.saturating_duration_since(at) >= NONREUSE_WINDOW)
    }

    /// The next Identification, or nothing when the cycle is spent and may not start again yet.
    fn issue(&mut self, now: Instant) -> Option<u16> {
        if self.issued >= CYCLE {
            if !self.reusable(now) {
                return None;
            }
            self.issued = 0;
        }
        self.issued += 1;
        // 1..=65535 and then 0, so the whole space is spent exactly once per cycle and `issued == CYCLE` is
        // the only thing that means "spent" - which a wrapping counter could never say.
        Some(self.issued as u16)
    }

    /// Gives back the sequence position of the value most recently issued, if that is still what this is.
    fn unissue(&mut self, identification: u16) -> bool {
        if self.issued == 0 || self.issued as u16 != identification {
            return false;
        }
        self.issued -= 1;
        true
    }
}

/// The one allocator, owned by the ingress task and shared by every producer.
pub struct Ipv4Identifications {
    entries: HashMap<Tuple, Entry>,
    capacity: usize,
    /// How many guarded packets may be accepted by the writer but not yet settled. Equal to the depth of the
    /// settlement channel, which is what makes a terminal impossible to lose to a full channel.
    tracked: usize,
    outstanding: usize,
    /// When this session's allocator opened, and so when its opening window started.
    opened: Instant,
    /// The earliest moment a full table may be scanned for reclaimable entries again.
    sweep_after: Instant,
    /// New tuples refused because the table was full. Counted rather than reported: which tuples arrive is
    /// traffic, so a line per refusal would be a flood by construction. The three below are the same, for the
    /// same reason.
    refused: u64,
    exhausted: u64,
    quarantined: u64,
    untracked: u64,
    reclaimed: u64,
    sweeps: u64,
    /// Sequence positions given back because no packet of that datagram was ever accepted - see
    /// [Ipv4Identifications::unissued].
    unissued: u64,
    written: u64,
    unwritten: u64,
    /// Settlements this table had no registration left to apply: a tuple it does not hold, or one holding
    /// nothing pending at all.
    stale: u64,
}

impl Ipv4Identifications {
    /// Prepares for `tuples` live tuples and never grows past it: a tuple beyond that logical maximum is
    /// refused, or takes the slot of one that can no longer collide, so what the bound allows is what was
    /// charged for and stays that way.
    pub fn new(prepared: Prepared) -> Self {
        let Prepared {
            tuples,
            tracked,
            opened,
        } = prepared;
        Self {
            entries: HashMap::with_capacity(tuples),
            capacity: tuples,
            tracked,
            outstanding: 0,
            opened,
            // The first sweep is permitted at once. Nothing can reach it any sooner than the opening window
            // allows a newcomer through, which is the same instant.
            sweep_after: opened,
            refused: 0,
            exhausted: 0,
            quarantined: 0,
            untracked: 0,
            reclaimed: 0,
            sweeps: 0,
            unissued: 0,
            written: 0,
            unwritten: 0,
            stale: 0,
        }
    }

    /// What a table prepared for `capacity` tuples owns, whatever is in it.
    pub fn footprint(capacity: usize) -> Option<u64> {
        logical_footprint::<(Tuple, Entry)>(capacity)?
            .checked_add(std::mem::size_of::<Self>() as u64)
    }

    /// The next Identification for this tuple, or the reason there is not one.
    pub fn next(&mut self, tuple: Tuple, now: Instant) -> Result<Guarded, Denial> {
        // Before anything else, and before the table is even consulted: inside the opening window there is no
        // value this session may issue, whichever tuple asks.
        if now.saturating_duration_since(self.opened) < NONREUSE_WINDOW {
            self.quarantined += 1;
            return Err(Denial::Quarantined);
        }
        // The borrow ends with this expression, so the counters below are reachable without a second lookup.
        let held = self.entries.get_mut(&tuple).map(|entry| {
            entry.issue(now).map(|identification| Guarded {
                tuple,
                identification,
            })
        });
        match held {
            Some(Some(guarded)) => return Ok(guarded),
            Some(None) => {
                self.exhausted += 1;
                return Err(Denial::Exhausted);
            }
            None => {}
        }
        // One condition for a new tuple: the logical maximum this table was charged row state for. A reclaim
        // gives a slot back and the newcomer that paid for the sweep may take it; what the container does with
        // its own backing is opaque count-bounded overhead and not consulted here.
        if self.entries.len() >= self.capacity {
            // A newcomer at a full table may pay for one scan, and only one: which tuples arrive is a
            // client's choice, so a scan per newcomer would be a whole-table walk an app could drive at will.
            if now < self.sweep_after {
                self.refused += 1;
                return Err(Denial::AtCapacity);
            }
            self.sweep(now);
            if self.entries.len() >= self.capacity {
                self.refused += 1;
                return Err(Denial::AtCapacity);
            }
        }
        // The bound has a free slot, so this row is one the aggregate was told about.
        self.entries.insert(
            tuple,
            Entry {
                issued: 1,
                pending: 0,
                written: None,
            },
        );
        Ok(Guarded {
            tuple,
            identification: 1,
        })
    }

    /// Gives back the sequence position of a datagram no packet of which was ever accepted.
    pub fn unissued(&mut self, guarded: Guarded) {
        let given_back = self
            .entries
            .get_mut(&guarded.tuple)
            .is_some_and(|entry| entry.unissue(guarded.identification));
        if given_back {
            self.unissued += 1;
        }
    }

    /// Gives away every slot whose occupant can no longer collide with anything.
    fn sweep(&mut self, now: Instant) {
        self.sweeps += 1;
        self.sweep_after = now.checked_add(NONREUSE_WINDOW).unwrap_or(now);
        let before = self.entries.len();
        self.entries.retain(|_, entry| !entry.reusable(now));
        self.reclaimed += (before - self.entries.len()) as u64;
    }

    /// Takes ownership of one packet of a guarded datagram, before the writer can be given it.
    pub fn register(&mut self, guarded: Guarded) -> Result<(), Untracked> {
        if self.outstanding >= self.tracked {
            self.untracked += 1;
            return Err(Untracked);
        }
        let taken = match self.entries.get_mut(&guarded.tuple) {
            Some(entry) => {
                entry.pending += 1;
                true
            }
            None => false,
        };
        if !taken {
            // Unreachable: a guarded identity is registered in the same call that issued it, and nothing
            // between the two can reclaim its bucket.
            self.stale += 1;
            return Err(Untracked);
        }
        self.outstanding += 1;
        Ok(())
    }

    /// Gives back a registration for a packet the writer refused, which is a packet it never owned.
    pub fn rolled_back(&mut self, guarded: Guarded) {
        if !self.release(guarded, |_| {}) {
            self.stale += 1;
        }
    }

    /// Applies one ending the writer sent back.
    pub fn terminal(&mut self, terminal: Terminal) {
        let Terminal { guarded, written } = terminal;
        let settled = self.release(guarded, |entry| {
            if let Some(at) = written {
                // The latest write, not the last terminal to arrive: two fragments of one datagram can be
                // settled in either order, and the window belongs to whichever of them reached the wire
                // last.
                if entry.written.is_none_or(|previous| previous < at) {
                    entry.written = Some(at);
                }
            }
        });
        if !settled {
            self.stale += 1;
            return;
        }
        match written {
            Some(_) => self.written += 1,
            None => self.unwritten += 1,
        }
    }

    /// Drops one registration and lets its ending touch the entry, answering whether it was this table's to
    /// drop at all.
    fn release(&mut self, guarded: Guarded, settle: impl FnOnce(&mut Entry)) -> bool {
        let released = match self.entries.get_mut(&guarded.tuple) {
            Some(entry) if entry.pending > 0 => {
                entry.pending -= 1;
                settle(entry);
                true
            }
            _ => false,
        };
        if released {
            self.outstanding -= 1;
        }
        released
    }

    /// How many new tuples the table has refused, so a session can say whether its capacity was ever reached.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// Datagrams denied because their tuple had spent its sequence and its window had not passed.
    pub fn exhausted(&self) -> u64 {
        self.exhausted
    }

    /// Datagrams denied because the session had not been open for [NONREUSE_WINDOW] yet.
    pub fn quarantined(&self) -> u64 {
        self.quarantined
    }

    /// Guarded packets not handed to the writer because their ending could not be tracked.
    pub fn untracked(&self) -> u64 {
        self.untracked
    }

    pub fn reclaimed(&self) -> u64 {
        self.reclaimed
    }

    pub fn sweeps(&self) -> u64 {
        self.sweeps
    }

    /// Sequence positions handed back because nothing carrying them was ever accepted.
    pub fn unissued_count(&self) -> u64 {
        self.unissued
    }

    /// Guarded packets the writer said reached the wire, and the ones it said did not.
    pub fn settled(&self) -> (u64, u64) {
        (self.written, self.unwritten)
    }

    /// Terminals that named nothing this table holds. Zero, or the settlement path lost something.
    pub fn stale(&self) -> u64 {
        self.stale
    }

    /// Guarded packets the writer has been given and has not settled.
    pub fn outstanding(&self) -> usize {
        self.outstanding
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn describe(&self) -> String {
        format!(
            "{} of {} identification tuples, {} of {} guarded packets unsettled, refused {} \
             exhausted {} quarantined {} untracked {}, {} sweeps reclaimed {}, {} unissued, \
             settled {} written {} unwritten, stale {}",
            self.entries.len(),
            self.capacity,
            self.outstanding,
            self.tracked,
            self.refused,
            self.exhausted,
            self.quarantined,
            self.untracked,
            self.sweeps,
            self.reclaimed,
            self.unissued,
            self.written,
            self.unwritten,
            self.stale,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::shared::admission::{largest_fitting, Headroom};

    const A: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
    const B: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
    const REMOTE: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 1);
    const UDP: u8 = 17;

    fn open_table(tuples: usize, tracked: usize) -> (Ipv4Identifications, Instant) {
        let opened = Instant::now();
        let table = Ipv4Identifications::new(Prepared {
            tuples,
            tracked,
            opened,
        });
        (table, opened + NONREUSE_WINDOW)
    }

    fn round_trip(table: &mut Ipv4Identifications, guarded: Guarded, written: Option<Instant>) {
        table.register(guarded).expect("tracked");
        table.terminal(match written {
            Some(at) => Terminal::wrote(guarded, at),
            None => Terminal::unwritten(guarded),
        });
    }

    #[test]
    fn a_tuple_spends_its_whole_sequence_once_and_then_stops() {
        let (mut table, now) = open_table(4, 8);
        let mut seen = vec![false; CYCLE as usize];
        for index in 0..CYCLE {
            let guarded = table.next((A, REMOTE, UDP), now).expect("issued");
            let identification = guarded.identification() as usize;
            assert!(
                !seen[identification],
                "{identification} issued twice at {index}"
            );
            seen[identification] = true;
            round_trip(&mut table, guarded, Some(now));
        }
        assert!(seen.into_iter().all(|used| used));
        assert_eq!(
            table.next((A, REMOTE, UDP), now),
            Err(Denial::Exhausted),
            "the 65,537th has no value left"
        );
        assert_eq!(table.exhausted(), 1);

        let other = table.next((B, REMOTE, UDP), now).expect("issued");
        assert_eq!(other.identification(), 1);
        assert_eq!(other.tuple(), (B, REMOTE, UDP));
        assert_eq!(
            table
                .next((A, REMOTE, 1), now)
                .expect("issued")
                .identification(),
            1
        );
    }

    #[test]
    fn a_sequence_that_never_reached_the_wire_restarts_immediately() {
        let (mut table, now) = open_table(4, 8);
        for _ in 0..CYCLE {
            let guarded = table.next((A, REMOTE, UDP), now).expect("issued");
            table.register(guarded).expect("tracked");
            table.rolled_back(guarded);
        }
        assert_eq!(
            table
                .next((A, REMOTE, UDP), now)
                .expect("restarted")
                .identification(),
            1
        );
        assert_eq!(table.exhausted(), 0);
        assert_eq!(table.settled(), (0, 0));
    }

    #[test]
    fn an_exhausted_tuple_waits_for_its_window_and_for_every_accepted_packet() {
        let (mut table, now) = open_table(4, 8);
        let mut last = None;
        for _ in 0..CYCLE {
            let guarded = table.next((A, REMOTE, UDP), now).expect("issued");
            last = Some(guarded);
        }
        let last = last.expect("a cycle");
        round_trip(&mut table, last, Some(now));

        assert_eq!(
            table.next(
                (A, REMOTE, UDP),
                now + NONREUSE_WINDOW - Duration::from_nanos(1)
            ),
            Err(Denial::Exhausted),
            "a nanosecond short of the window"
        );
        assert_eq!(
            table
                .next((A, REMOTE, UDP), now + NONREUSE_WINDOW)
                .expect("restarted")
                .identification(),
            1,
            "and open at exactly the window"
        );
    }

    #[test]
    fn a_pending_packet_blocks_reuse_whatever_the_clock_says() {
        let (mut table, now) = open_table(4, 8);
        let mut held = None;
        for index in 0..CYCLE {
            let guarded = table.next((A, REMOTE, UDP), now).expect("issued");
            if index == 0 {
                table.register(guarded).expect("tracked");
                held = Some(guarded);
            }
        }
        let held = held.expect("one held");
        let hour = now + Duration::from_secs(3_600);
        assert_eq!(
            table.next((A, REMOTE, UDP), hour),
            Err(Denial::Exhausted),
            "an hour is not enough while a packet is unaccounted for"
        );
        table.terminal(Terminal::unwritten(held));
        assert_eq!(
            table
                .next((A, REMOTE, UDP), hour)
                .expect("restarted")
                .identification(),
            1
        );
        assert_eq!(table.outstanding(), 0);
    }

    #[test]
    fn the_latest_written_fragment_sets_the_window() {
        let (mut table, now) = open_table(4, 8);
        let mut last = None;
        for _ in 0..CYCLE {
            last = Some(table.next((A, REMOTE, UDP), now).expect("issued"));
        }
        let guarded = last.expect("a cycle");
        table.register(guarded).expect("tracked");
        table.register(guarded).expect("tracked");
        let later = now + Duration::from_secs(30);
        table.terminal(Terminal::wrote(guarded, later));
        table.terminal(Terminal::wrote(guarded, now));
        assert_eq!(table.settled(), (2, 0));
        assert_eq!(table.stale(), 0);

        assert_eq!(
            table.next(
                (A, REMOTE, UDP),
                later + NONREUSE_WINDOW - Duration::from_nanos(1)
            ),
            Err(Denial::Exhausted),
            "the earlier write must not shorten the window"
        );
        assert_eq!(
            table
                .next((A, REMOTE, UDP), later + NONREUSE_WINDOW)
                .expect("restarted")
                .identification(),
            1
        );
    }

    #[test]
    fn the_opening_window_denies_every_guarded_datagram() {
        let opened = Instant::now();
        let mut table = Ipv4Identifications::new(Prepared {
            tuples: 4,
            tracked: 8,
            opened,
        });
        for elapsed in [
            Duration::ZERO,
            Duration::from_secs(30),
            NONREUSE_WINDOW - Duration::from_nanos(1),
        ] {
            assert_eq!(
                table.next((A, REMOTE, UDP), opened + elapsed),
                Err(Denial::Quarantined),
                "{elapsed:?} into the session"
            );
        }
        assert_eq!(table.quarantined(), 3);
        assert!(table.is_empty());
        assert_eq!(table.refused(), 0);
        assert_eq!(
            table
                .next((A, REMOTE, UDP), opened + NONREUSE_WINDOW)
                .expect("open")
                .identification(),
            1
        );
    }

    #[test]
    fn a_fresh_session_does_not_restart_where_the_last_one_stopped() {
        let (mut first, now) = open_table(4, 8);
        let guarded = first.next((A, REMOTE, UDP), now).expect("issued");
        round_trip(&mut first, guarded, Some(now));
        assert_eq!(guarded.identification(), 1);

        let successor = now + Duration::from_millis(1);
        let mut second = Ipv4Identifications::new(Prepared {
            tuples: 4,
            tracked: 8,
            opened: successor,
        });
        assert_eq!(
            second.next((A, REMOTE, UDP), successor),
            Err(Denial::Quarantined)
        );
        assert_eq!(
            second.next(
                (A, REMOTE, UDP),
                successor + NONREUSE_WINDOW - Duration::from_nanos(1)
            ),
            Err(Denial::Quarantined)
        );
        assert!(second.is_empty());
    }

    #[test]
    fn a_full_table_reclaims_only_what_can_no_longer_collide() {
        let (mut table, now) = open_table(2, 8);
        let busy = table.next((A, REMOTE, UDP), now).expect("issued");
        table.register(busy).expect("tracked");
        let quiet = table.next((B, REMOTE, UDP), now).expect("issued");
        round_trip(&mut table, quiet, Some(now));
        let charged = Ipv4Identifications::footprint(2).expect("chargeable");

        let newcomer = (Ipv4Addr::new(192, 0, 2, 3), REMOTE, UDP);
        assert_eq!(
            table.next(newcomer, now + Duration::from_secs(30)),
            Err(Denial::AtCapacity)
        );
        assert_eq!(table.sweeps(), 1);
        assert_eq!(table.reclaimed(), 0);
        assert_eq!(table.len(), 2);

        let later = now + Duration::from_secs(90);
        let taken = table.next(newcomer, later).expect("took the freed slot");
        assert_eq!(
            taken.identification(),
            1,
            "a newcomer starts a sequence of its own"
        );
        assert_eq!(table.len(), 2);
        assert_eq!(table.reclaimed(), 1);
        assert_eq!(
            Ipv4Identifications::footprint(table.capacity()).expect("chargeable"),
            charged,
            "and a reclaim is not a resize: the bound, and so the charge, is where it was"
        );

        assert_eq!(
            table
                .next((A, REMOTE, UDP), later)
                .expect("still here")
                .identification(),
            2
        );
    }

    #[test]
    fn hostile_newcomers_between_sweeps_stay_quiet_and_cheap() {
        let (mut table, now) = open_table(2, 8);
        for source in [A, B] {
            let guarded = table.next((source, REMOTE, UDP), now).expect("issued");
            round_trip(&mut table, guarded, Some(now));
        }
        assert_eq!(
            table.next((Ipv4Addr::new(203, 0, 113, 1), REMOTE, UDP), now),
            Err(Denial::AtCapacity)
        );
        assert_eq!(table.sweeps(), 1);
        for index in 0..10_000u32 {
            let source = Ipv4Addr::from((index + 1_000).to_be_bytes());
            assert_eq!(
                table.next((source, REMOTE, UDP), now),
                Err(Denial::AtCapacity)
            );
        }
        assert_eq!(table.sweeps(), 1);
        assert_eq!(table.refused(), 10_001);
        assert_eq!(table.len(), 2);
        assert_eq!(table.reclaimed(), 0);
        for expected in 2..=6u16 {
            for source in [A, B] {
                assert_eq!(
                    table
                        .next((source, REMOTE, UDP), now)
                        .expect("held")
                        .identification(),
                    expected
                );
            }
        }
    }

    #[test]
    fn a_reclaim_gives_a_slot_back_to_the_newcomer_that_paid_for_the_sweep() {
        let capacity = 8usize;
        let (mut table, start) = open_table(capacity, 8);
        let charged = Ipv4Identifications::footprint(capacity).expect("chargeable");
        let mut now = start;

        for index in 0..capacity {
            let source = Ipv4Addr::from((index as u32).to_be_bytes());
            let guarded = table
                .next((source, REMOTE, UDP), now)
                .unwrap_or_else(|why| panic!("tuple {index}: {why:?}"));
            round_trip(&mut table, guarded, Some(now));
        }
        assert_eq!(table.len(), capacity);

        let newcomer = (Ipv4Addr::new(203, 0, 113, 1), REMOTE, UDP);
        assert_eq!(table.next(newcomer, now), Err(Denial::AtCapacity));
        assert_eq!(table.reclaimed(), 0);

        now += NONREUSE_WINDOW + Duration::from_secs(1);
        let guarded = table.next(newcomer, now).expect("a reclaimed slot");
        assert!(table.reclaimed() > 0);
        assert_eq!(
            guarded.identification(),
            1,
            "and the newcomer holding one starts a sequence of its own"
        );
        round_trip(&mut table, guarded, Some(now));
        assert!(table.len() <= capacity);
        assert_eq!(
            Ipv4Identifications::footprint(table.capacity()).expect("chargeable"),
            charged,
            "so the charge is still what its rows cost"
        );
    }

    #[test]
    fn a_settlement_with_nothing_outstanding_is_counted_and_applied_to_nothing() {
        let (mut table, now) = open_table(2, 8);
        let live = table.next((A, REMOTE, UDP), now).expect("issued");
        table.register(live).expect("tracked");

        let unregistered = table.next((B, REMOTE, UDP), now).expect("issued");
        table.terminal(Terminal::wrote(unregistered, now));
        assert_eq!(table.stale(), 1);
        assert_eq!(table.outstanding(), 1);
        assert_eq!(table.settled(), (0, 0));

        table.terminal(Terminal::wrote(live, now));
        assert_eq!(table.outstanding(), 0);
        assert_eq!(table.settled(), (1, 0));
        table.terminal(Terminal::wrote(live, now));
        assert_eq!(table.stale(), 2);
        assert_eq!(table.settled(), (1, 0));
        assert_eq!(table.outstanding(), 0);
    }

    #[test]
    fn a_datagram_nothing_accepted_gives_its_position_back() {
        let (mut table, now) = open_table(4, 8);
        let first = table.next((A, REMOTE, UDP), now).expect("issued");
        assert_eq!(first.identification(), 1);
        table.unissued(first);
        assert_eq!(table.unissued_count(), 1);

        let again = table.next((A, REMOTE, UDP), now).expect("issued");
        assert_eq!(again, first);

        round_trip(&mut table, again, Some(now));
        assert_eq!(
            table
                .next((A, REMOTE, UDP), now)
                .expect("issued")
                .identification(),
            2
        );
        assert_eq!(
            table.unissued_count(),
            1,
            "exactly the one that reached nothing"
        );
    }

    #[test]
    fn rollback_reaches_only_the_latest_issuance() {
        let (mut table, now) = open_table(4, 8);
        let committed = table.next((A, REMOTE, UDP), now).expect("issued");
        table.register(committed).expect("tracked");
        let latest = table.next((A, REMOTE, UDP), now).expect("issued");
        assert_eq!(latest.identification(), 2);

        table.unissued(committed);
        assert_eq!(table.unissued_count(), 0);
        assert_eq!(
            table
                .next((A, REMOTE, UDP), now)
                .expect("issued")
                .identification(),
            3,
            "the sequence carried on past both"
        );

        table.unissued(Guarded {
            tuple: (B, REMOTE, UDP),
            identification: 1,
        });
        assert_eq!(table.unissued_count(), 0);
        assert_eq!(latest.identification(), 2);
    }

    #[test]
    fn attempts_that_reach_nothing_never_spend_the_cycle() {
        let (mut table, now) = open_table(4, 8);
        let real = table.next((A, REMOTE, UDP), now).expect("issued");
        round_trip(&mut table, real, Some(now));
        for _ in 0..CYCLE + 16 {
            let attempt = table.next((A, REMOTE, UDP), now).expect("issued");
            table.unissued(attempt);
        }
        assert_eq!(table.exhausted(), 0);
        assert_eq!(
            table
                .next((A, REMOTE, UDP), now)
                .expect("issued")
                .identification(),
            2,
            "and the sequence is still where the one real datagram left it"
        );
    }

    #[test]
    fn the_tracking_bound_refuses_rather_than_losing_an_ending() {
        let (mut table, now) = open_table(4, 3);
        let held: Vec<_> = (0..3)
            .map(|_| table.next((A, REMOTE, UDP), now).expect("issued"))
            .collect();
        for guarded in &held {
            table.register(*guarded).expect("tracked");
        }
        assert_eq!(table.outstanding(), 3);
        let extra = table.next((A, REMOTE, UDP), now).expect("issued");
        assert_eq!(table.register(extra), Err(Untracked));
        assert_eq!(table.untracked(), 1);
        assert_eq!(table.outstanding(), 3);

        table.terminal(Terminal::wrote(held[0], now));
        assert_eq!(table.outstanding(), 2);
        assert_eq!(table.register(extra), Ok(()));
        assert_eq!(table.outstanding(), 3);
    }

    #[test]
    fn a_prepared_table_admits_its_whole_bound() {
        for capacity in [0usize, 1, 3, 8, 64, 1_000] {
            let (mut table, now) = open_table(capacity, 8);
            assert_eq!(
                table.entries.len() < table.capacity,
                capacity > 0,
                "a table prepared for {capacity} takes a first tuple exactly when there is one to take"
            );
            for index in 0..capacity {
                let source = Ipv4Addr::from((index as u32).to_be_bytes());
                let guarded = table.next((source, REMOTE, UDP), now).expect("issued");
                assert_eq!(guarded.identification(), 1);
                round_trip(&mut table, guarded, Some(now));
            }
            assert_eq!(table.len(), capacity);
            assert_eq!(
                table.next((Ipv4Addr::new(203, 0, 113, 1), REMOTE, UDP), now),
                Err(Denial::AtCapacity)
            );
            assert_eq!(table.len(), capacity);

            let charged = Ipv4Identifications::footprint(capacity).expect("chargeable");
            assert_eq!(
                charged,
                capacity as u64 * std::mem::size_of::<(Tuple, Entry)>() as u64
                    + std::mem::size_of::<Ipv4Identifications>() as u64,
                "capacity {capacity}"
            );
        }
        assert_eq!(Ipv4Identifications::footprint(usize::MAX), None);
    }

    #[test]
    fn the_solved_capacity_is_the_largest_that_fits() {
        for bytes in [1_024u64, 64 * 1_024, 4 * 1_024 * 1_024] {
            let tuples = largest_fitting(
                Headroom {
                    records: u32::MAX,
                    bytes,
                },
                0,
                Ipv4Identifications::footprint,
            );
            let charged = Ipv4Identifications::footprint(tuples).expect("chargeable");
            assert!(
                charged <= bytes,
                "{tuples} tuples cost {charged} of {bytes}"
            );
            let past = Ipv4Identifications::footprint(tuples + 1).expect("chargeable");
            assert!(past > bytes, "{} tuples would also fit", tuples + 1);
            let (mut table, now) = open_table(tuples, 8);
            for index in 0..tuples.min(64) {
                assert!(
                    table.entries.len() < table.capacity,
                    "tuple {index} of {tuples}"
                );
                let source = Ipv4Addr::from((index as u32).to_be_bytes());
                table.next((source, REMOTE, UDP), now).expect("issued");
            }
        }
    }
}
