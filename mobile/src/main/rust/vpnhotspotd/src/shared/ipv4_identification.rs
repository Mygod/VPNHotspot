//! The guarded IPv4 Identification allocator: which value one oversized datagram may carry, and when that
//! value may be used for that tuple again.
//!
//! A receiver reassembles on source, destination, protocol, and Identification, and holds the pieces for up
//! to sixty seconds. So the property this owner exists for is about the *wire*: the same
//! `(source, destination, protocol, Identification)` must not reach the TUN twice inside that window, or two
//! unrelated datagrams mis-splice into one and the client cannot tell.
//!
//! Three things follow from that sentence, and they are why this is a temporal table rather than a map of
//! counters.
//!
//! - **A sequence is not enough on its own.** A per-tuple counter cannot collide until it wraps, but a
//!   sixteen-bit counter *does* wrap, and the datagram after the 65,536th would carry a value that may still
//!   be inside a receiver's window. So the cycle ends rather than wrapping, and only time reopens it. What
//!   ending a cycle costs is availability, which is why issuing is a transaction: a datagram that put
//!   nothing carrying its value anywhere gives that value back rather than spending it.
//! - **Allocation time is not wire time.** The TUN writer holds a bounded queue and may be parked waiting for
//!   the kernel to accept a write for as long as the kernel likes, so a packet allocated first can reach the
//!   wire arbitrarily later. Only the writer knows whether a packet was written and when, which is why the
//!   window here is driven by a terminal the writer sends back - see [Terminal] - and never by the moment an
//!   Identification was handed out.
//! - **A previous session's writes are invisible to this one.** A fresh table starts every sequence from the
//!   beginning, and a daemon restarted a second after it stopped would hand out values its predecessor had
//!   just put on the wire. So a session denies every guarded datagram for its first [NONREUSE_WINDOW],
//!   which is exactly as long as any receiver may still be holding what the last session sent.
//!
//! Bounded, because the tuple set is attacker-influenced: any local app can source packets, per the security
//! posture, so an unbounded map would be a memory hole. Bounded *and* reclaimable, because a table that only
//! ever filled would deny every new tuple for the rest of the session - and reclaim is safe for exactly the
//! entries a restart would be safe for, which is the same test in both places.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crate::shared::admission::logical_footprint;

/// How long a receiver may hold fragments of one datagram, and therefore how long an Identification stays
/// dangerous after it reaches the wire.
///
/// RFC 8200 section 4.5 requires sixty seconds for IPv6 and RFC 791 allows up to it for IPv4, which is the
/// same figure `shared/reassembly.rs` holds this daemon's own contexts for. Taken as the whole window rather
/// than reduced by anything: what a downstream receiver does is not this daemon's to shorten.
pub const NONREUSE_WINDOW: Duration = Duration::from_secs(60);

/// How many Identifications one tuple's sequence has before it is spent.
///
/// The whole sixteen-bit space, each value once. Not a wrap: the value after the last one is no value at all
/// until time says otherwise.
const CYCLE: u32 = 1 << 16;

/// What a receiver reassembles on, minus the Identification this hands out.
pub type Tuple = (Ipv4Addr, Ipv4Addr, u8);

/// One guarded datagram's identity, issued before it is built and carried by every packet it becomes.
///
/// Opaque on purpose. What a producer needs from it is the Identification to put in a header, and what the
/// writer needs is something to hand back when the packet ends. One datagram gets one of these however many
/// fragments it turns into, which is what makes those fragments reassemble into one datagram rather than
/// several.
///
/// The tuple and the value are the whole identity. A bucket is reclaimed only when its occupant has no packet
/// outstanding, and every accepted packet remains outstanding until its terminal arrives, so no terminal can
/// outlive the entry it names. A settlement for a tuple with nothing pending is not this table's to apply.
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
///
/// While the session continues, every packet the writer accepted ends in exactly one of these, and the
/// distinction it carries is the whole point: a packet that reached the wire starts a sixty-second window for
/// its tuple, and one that did not - dropped at the dequeue stamp gate, refused by final validation, or
/// abandoned because a retirement preempted its blocked write - starts nothing, because there is nothing out
/// there to collide with. The endings of the session itself settle nothing, because by then this table is
/// about to be dropped and it is the successor's opening window that covers what those packets did.
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
///
/// Three reasons rather than one, because they say different things about the daemon: the first is this
/// session being younger than a receiver's memory, the second is a bounded table doing its job, and the third
/// is one tuple having spent its whole sequence faster than the window it has to wait out. All three are
/// answered the same way by the caller - the datagram is dropped, quietly and counted, before any header is
/// built - and all three are counted apart so a session can say which it was.
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
///
/// The settlement path is bounded like everything else, and a guarded packet whose terminal has nowhere to go
/// is one whose wire time this table would never learn - which is the one thing it must not guess at. So the
/// packet is dropped exactly as a full writer queue would drop it, rather than being sent untracked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Untracked;

/// What one session's allocator is prepared for.
///
/// Grouped because the three are decided together and none of them is meaningful alone: how many tuples the
/// table holds comes from a share of the byte budget, how many guarded packets may be unsettled at once comes
/// from the writer's real queue depth, and when the session opened is what the opening window is measured
/// from.
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
    ///
    /// One test for both, because they are the same question asked twice: a restart hands out values this
    /// tuple has used before, and a reclaim hands the bucket to a newcomer whose own sequence then starts at
    /// the beginning of the same value space. Neither is safe while a packet of this tuple is unaccounted
    /// for, and neither is safe while anything it wrote could still be in a receiver's hands.
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
    ///
    /// Answers whether it did. The guard is the identity: `issued` is what produced `identification`, so
    /// comparing them is asking "is this still the last thing I handed out" - and anything else, including a
    /// second attempt at the same rollback, leaves the sequence alone.
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
    ///
    /// Narrower than "a duplicated ending", and deliberately so. The count is per tuple, not per packet, so
    /// a second copy of one datagram's ending is caught only when nothing else of that tuple is outstanding;
    /// while another packet of the same tuple is pending it would be applied to that one instead. The writer
    /// sends one settlement per accepted packet, and this aggregate catches settlements that name no pending
    /// packet.
    stale: u64,
}

impl Ipv4Identifications {
    /// Prepares for `tuples` live tuples and never grows past it: a tuple beyond that logical maximum is
    /// refused, or takes the slot of one that can no longer collide, so what the bound allows is what was
    /// charged for and stays that way.
    ///
    /// `with_capacity` requests those rows up front so the common case allocates nothing - see
    /// [Ipv4Identifications::footprint] for what is charged. The container may allocate or reorganise its own
    /// backing from there; that is count-bounded overhead rather than accounted state, and
    /// [Ipv4Identifications::next] refuses on the bound alone.
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
    ///
    /// Charged once, as one table owner, rather than a record per tuple. A tuple holds no descriptor, so
    /// spending an aggregate *record* on each - which is what the descriptor total measures - would let a
    /// client that talks to many destinations exhaust the budget for mappings and flows with a few bytes of
    /// counter apiece.
    ///
    /// The rows and this struct. This owner is a pure table, so it is the one place where the container
    /// overhead left out of [logical_footprint] is the *dominant* uncharged term rather than a rounding error
    /// beside real buffers: the real allocation for `capacity` rows exceeds this figure by whatever `std`'s
    /// hash container keeps around them, which it does not document and this does not guess at.
    ///
    /// So what the sixteenth of the dataplane's measured share in [crate::tun_reader] bounds is this table's
    /// *cardinality* - how many tuples may exist at once - by way of the row state charged here. It is a
    /// policy budget for that charged state and not a claim about the backing, which scales with the same
    /// count and stays unquantified. See the Resource Policy note on what the byte total does and does not
    /// claim.
    pub fn footprint(capacity: usize) -> Option<u64> {
        logical_footprint::<(Tuple, Entry)>(capacity)?
            .checked_add(std::mem::size_of::<Self>() as u64)
    }

    /// The next Identification for this tuple, or the reason there is not one.
    ///
    /// Nothing here allocates outside the prepared capacity and nothing here scans the table except a sweep
    /// that may run at most once per [NONREUSE_WINDOW], so a client driving newcomers at a full table costs
    /// one hash lookup and one counter each.
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
    ///
    /// The counterpart to [Ipv4Identifications::next] issuing before the header exists, which it has to: the
    /// value goes *into* the packet, so it cannot be decided afterwards. What that leaves is a datagram that
    /// spent a position and then failed to build, or whose every packet the writer refused - and treating
    /// that as spent is what let a tuple be pushed into denial by 65,536 attempts that never reached the
    /// wire at all, which a client can arrange by keeping the queue full. Nothing carries the value, so
    /// handing it out again is exactly as safe as handing it out was.
    ///
    /// Only the latest issuance, and only when it is still the latest, so a value some later datagram has
    /// already moved past is left alone. Whether anything *carries* the value is the caller's to know rather
    /// than this table's: [crate::shared::packet_writer::emit] is the only caller and asks only when the
    /// sink accepted nothing at all, so a fragmented datagram of which even one fragment was accepted keeps
    /// its value. The ingress owner is serialized, so between issuing and this there is nothing else that
    /// could have issued.
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
    ///
    /// The charge is row state for a bound a sweep does not move, so a reclaim refunds nothing and asks for
    /// nothing. What it does give back is *logical* slots, which is why
    /// [Ipv4Identifications::next] re-checks its bound after sweeping: whether the sweep found anything old
    /// enough decides whether the newcomer that paid for it gets in.
    fn sweep(&mut self, now: Instant) {
        self.sweeps += 1;
        self.sweep_after = now.checked_add(NONREUSE_WINDOW).unwrap_or(now);
        let before = self.entries.len();
        self.entries.retain(|_, entry| !entry.reusable(now));
        self.reclaimed += (before - self.entries.len()) as u64;
    }

    /// Takes ownership of one packet of a guarded datagram, before the writer can be given it.
    ///
    /// Before, and not after, because the terminal for a packet the writer already has may arrive at any
    /// moment: a count incremented afterwards could be decremented first. Refuses when the settlement path is
    /// already holding as many endings as it can, which the caller treats exactly as a full writer queue.
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
    ///
    /// Not a terminal: nothing reached the wire and nothing claims to have, so this touches the tuple's window
    /// not at all.
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
    ///
    /// One lookup and one check for both callers, because "is there a registration to give back" is the same
    /// question whether the packet was refused before the writer had it or ended after the writer did. The
    /// check is `pending > 0`, which the decrement needs anyway, and it is an aggregate over the tuple rather
    /// than an identity: it stops a settlement arriving for a tuple with nothing outstanding, and it does not
    /// distinguish which of several pending packets a settlement belongs to. It does not need to - reuse
    /// waits for the count to reach zero and for the latest write to age out, and neither of those depends on
    /// which packet was which.
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

    /// A table whose opening window has already passed, so a test that is not about the window does not have
    /// to keep stepping over it.
    fn open_table(tuples: usize, tracked: usize) -> (Ipv4Identifications, Instant) {
        let opened = Instant::now();
        let table = Ipv4Identifications::new(Prepared {
            tuples,
            tracked,
            opened,
        });
        (table, opened + NONREUSE_WINDOW)
    }

    /// One guarded packet's whole life through the writer, as production runs it: registered before the writer
    /// could have it, and settled with or without a wire write.
    fn round_trip(table: &mut Ipv4Identifications, guarded: Guarded, written: Option<Instant>) {
        table.register(guarded).expect("tracked");
        table.terminal(match written {
            Some(at) => Terminal::wrote(guarded, at),
            None => Terminal::unwritten(guarded),
        });
    }

    /// Every one of the 65,536 values exactly once, and then nothing - where a wrapping counter would have
    /// handed the 65,537th datagram a value it had just used.
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
            // Each of them reaches the wire, which is what makes the window below real.
            round_trip(&mut table, guarded, Some(now));
        }
        assert!(seen.into_iter().all(|used| used), "the whole space");
        assert_eq!(
            table.next((A, REMOTE, UDP), now),
            Err(Denial::Exhausted),
            "the 65,537th has no value left"
        );
        assert_eq!(table.exhausted(), 1);

        // A different tuple is a different reassembly key, so it may hold the same values at the same moment.
        let other = table.next((B, REMOTE, UDP), now).expect("issued");
        assert_eq!(other.identification(), 1);
        assert_eq!(other.tuple(), (B, REMOTE, UDP));
        // And so is the same pair under a different protocol.
        assert_eq!(
            table
                .next((A, REMOTE, 1), now)
                .expect("issued")
                .identification(),
            1
        );
    }

    /// A cycle nothing reached the wire from may start again at once: there is nothing out there to collide
    /// with, so the window has nothing to protect.
    #[test]
    fn a_sequence_that_never_reached_the_wire_restarts_immediately() {
        let (mut table, now) = open_table(4, 8);
        for _ in 0..CYCLE {
            let guarded = table.next((A, REMOTE, UDP), now).expect("issued");
            // Accepted by nothing: the writer never owned it, so it is rolled back rather than settled.
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
        assert_eq!(table.settled(), (0, 0), "nothing was ever settled");
    }

    /// The window opens at exactly sixty seconds after the *latest* write, and not before - and a packet the
    /// writer still has blocks it however long ago its Identification was issued.
    #[test]
    fn an_exhausted_tuple_waits_for_its_window_and_for_every_accepted_packet() {
        let (mut table, now) = open_table(4, 8);
        let mut last = None;
        for _ in 0..CYCLE {
            let guarded = table.next((A, REMOTE, UDP), now).expect("issued");
            last = Some(guarded);
        }
        let last = last.expect("a cycle");
        // Every one but the last is settled without a write; the last one is the one that reaches the wire.
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

    /// Time alone is never enough: a packet the writer accepted and has not settled holds the sequence shut
    /// however long ago it was allocated, because its wire time is still unknown.
    #[test]
    fn a_pending_packet_blocks_reuse_whatever_the_clock_says() {
        let (mut table, now) = open_table(4, 8);
        let mut held = None;
        for index in 0..CYCLE {
            let guarded = table.next((A, REMOTE, UDP), now).expect("issued");
            if index == 0 {
                // Accepted into the writer's queue and never settled - parked behind a kernel that will not
                // take it.
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
        // It turns out never to have been written, so it starts no window of its own.
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

    /// One datagram's fragments settle in whatever order the writer gets to them, and the window belongs to
    /// the latest write among them rather than the last terminal to arrive.
    #[test]
    fn the_latest_written_fragment_sets_the_window() {
        let (mut table, now) = open_table(4, 8);
        let mut last = None;
        for _ in 0..CYCLE {
            last = Some(table.next((A, REMOTE, UDP), now).expect("issued"));
        }
        // One identity, registered twice: the same guarded datagram in two fragments, which is what makes
        // them reassemble into one datagram rather than two.
        let guarded = last.expect("a cycle");
        table.register(guarded).expect("tracked");
        table.register(guarded).expect("tracked");
        let later = now + Duration::from_secs(30);
        // Out of order: the later write is settled first.
        table.terminal(Terminal::wrote(guarded, later));
        table.terminal(Terminal::wrote(guarded, now));
        assert_eq!(table.settled(), (2, 0));
        assert_eq!(table.stale(), 0, "both named a live entry");

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

    /// A session denies everything guarded until it has been open as long as a receiver remembers, and opens
    /// at exactly that moment.
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
        assert!(table.is_empty(), "and the table was never touched");
        assert_eq!(table.refused(), 0, "which is not a capacity refusal");
        assert_eq!(
            table
                .next((A, REMOTE, UDP), opened + NONREUSE_WINDOW)
                .expect("open")
                .identification(),
            1
        );
    }

    /// A successor session starts its own window rather than its predecessor's sequence, which is the whole
    /// reason the window exists at all.
    #[test]
    fn a_fresh_session_does_not_restart_where_the_last_one_stopped() {
        let (mut first, now) = open_table(4, 8);
        let guarded = first.next((A, REMOTE, UDP), now).expect("issued");
        round_trip(&mut first, guarded, Some(now));
        assert_eq!(guarded.identification(), 1);

        // The daemon is handed a new TUN a moment later. The value above may still be in a receiver's hands.
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

    /// A full table gives away only what cannot collide, keeps every live entry exactly as it was, and
    /// charges nothing for doing it.
    ///
    /// The logical bound is the whole admission condition, so the slot the sweep gives back is one the
    /// newcomer takes. What the map does with its own backing is not consulted and not asserted.
    #[test]
    fn a_full_table_reclaims_only_what_can_no_longer_collide() {
        let (mut table, now) = open_table(2, 8);
        // One tuple with a packet still in the writer, one that wrote and went quiet.
        let busy = table.next((A, REMOTE, UDP), now).expect("issued");
        table.register(busy).expect("tracked");
        let quiet = table.next((B, REMOTE, UDP), now).expect("issued");
        round_trip(&mut table, quiet, Some(now));
        let charged = Ipv4Identifications::footprint(2).expect("chargeable");

        // Thirty seconds in, a sweep is permitted and finds nothing it may take.
        let newcomer = (Ipv4Addr::new(192, 0, 2, 3), REMOTE, UDP);
        assert_eq!(
            table.next(newcomer, now + Duration::from_secs(30)),
            Err(Denial::AtCapacity)
        );
        assert_eq!(table.sweeps(), 1);
        assert_eq!(table.reclaimed(), 0, "neither entry was old enough");
        assert_eq!(table.len(), 2);

        // A minute after that sweep the next one is permitted, the quiet tuple's window has passed, and its
        // slot goes to the newcomer.
        let later = now + Duration::from_secs(90);
        let taken = table.next(newcomer, later).expect("took the freed slot");
        assert_eq!(
            taken.identification(),
            1,
            "a newcomer starts a sequence of its own"
        );
        assert_eq!(table.len(), 2, "one in, one out");
        // And the sweep took the quiet tuple, only the quiet tuple.
        assert_eq!(table.reclaimed(), 1, "one slot given away, and one only");
        assert_eq!(
            Ipv4Identifications::footprint(table.capacity()).expect("chargeable"),
            charged,
            "and a reclaim is not a resize: the bound, and so the charge, is where it was"
        );

        // The busy tuple was never a candidate, and its sequence carries straight on from where it was.
        assert_eq!(
            table
                .next((A, REMOTE, UDP), later)
                .expect("still here")
                .identification(),
            2
        );
    }

    /// Newcomers arriving between permitted sweeps cost one lookup and one counter each - no scan, no
    /// allocation, no report, and nothing displaced.
    #[test]
    fn hostile_newcomers_between_sweeps_stay_quiet_and_cheap() {
        let (mut table, now) = open_table(2, 8);
        for source in [A, B] {
            let guarded = table.next((source, REMOTE, UDP), now).expect("issued");
            round_trip(&mut table, guarded, Some(now));
        }
        // The first newcomer pays for the one sweep this window allows.
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
        assert_eq!(table.sweeps(), 1, "one scan, however many newcomers");
        assert_eq!(table.refused(), 10_001);
        assert_eq!(table.len(), 2, "nothing was displaced");
        assert_eq!(table.reclaimed(), 0, "and nothing was taken from anyone");
        // The tuples that were already here carry straight on, monotonically.
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

    /// A reclaim gives bound slots back and the newcomer that paid for the sweep takes one.
    ///
    /// The ordinary shape: a full table refuses, a sweep hands back the slots whose occupants can no longer
    /// collide, and the newcomer that paid for that sweep gets in. The bound is the only admission condition,
    /// so a freed slot is an available slot - see [Ipv4Identifications::next].
    #[test]
    fn a_reclaim_gives_a_slot_back_to_the_newcomer_that_paid_for_the_sweep() {
        let capacity = 8usize;
        let (mut table, start) = open_table(capacity, 8);
        let charged = Ipv4Identifications::footprint(capacity).expect("chargeable");
        let mut now = start;

        // Fill the bound, and put every value on the wire so each tuple starts its own window.
        for index in 0..capacity {
            let source = Ipv4Addr::from((index as u32).to_be_bytes());
            let guarded = table
                .next((source, REMOTE, UDP), now)
                .unwrap_or_else(|why| panic!("tuple {index}: {why:?}"));
            round_trip(&mut table, guarded, Some(now));
        }
        assert_eq!(table.len(), capacity, "the bound is full");

        // A newcomer now is refused: it pays for the one sweep this window allows, and that sweep finds
        // nothing old enough to take.
        let newcomer = (Ipv4Addr::new(203, 0, 113, 1), REMOTE, UDP);
        assert_eq!(table.next(newcomer, now), Err(Denial::AtCapacity));
        assert_eq!(table.reclaimed(), 0, "nothing was old enough yet");

        // Once the window has passed, the next sweep gives slots back and the newcomer takes one.
        now += NONREUSE_WINDOW + Duration::from_secs(1);
        let guarded = table.next(newcomer, now).expect("a reclaimed slot");
        assert!(table.reclaimed() > 0, "slots really were given away");
        assert_eq!(
            guarded.identification(),
            1,
            "and the newcomer holding one starts a sequence of its own"
        );
        round_trip(&mut table, guarded, Some(now));
        assert!(table.len() <= capacity, "the live bound held");
        assert_eq!(
            Ipv4Identifications::footprint(table.capacity()).expect("chargeable"),
            charged,
            "so the charge is still what its rows cost"
        );
    }

    /// A settlement this table has no live registration for is counted and applied to nothing.
    ///
    /// Two shapes, and between them they are what an aggregate check can recognise: a tuple the table does
    /// not hold, and one it holds with nothing outstanding. What it cannot recognise is a duplicate arriving
    /// while some *other* packet of the same tuple is still pending, which would be applied to that one -
    /// and telling those apart would need an identity per registration rather than a count per tuple.
    ///
    /// That identity is not carried, because neither shape can arise while the writer is behaving: it sends
    /// one settlement per packet it accepted, and a bucket is reclaimed only when its occupant has none
    /// outstanding. The check earns its place by being the comparison the decrement needs anyway.
    #[test]
    fn a_settlement_with_nothing_outstanding_is_counted_and_applied_to_nothing() {
        let (mut table, now) = open_table(2, 8);
        let live = table.next((A, REMOTE, UDP), now).expect("issued");
        table.register(live).expect("tracked");

        // A tuple that has been issued a value but never registered a packet for it.
        let unregistered = table.next((B, REMOTE, UDP), now).expect("issued");
        table.terminal(Terminal::wrote(unregistered, now));
        assert_eq!(table.stale(), 1, "B holds an entry but nothing pending");
        assert_eq!(table.outstanding(), 1, "the live registration is untouched");
        assert_eq!(table.settled(), (0, 0));

        // And a duplicate of a settlement already applied.
        table.terminal(Terminal::wrote(live, now));
        assert_eq!(table.outstanding(), 0);
        assert_eq!(table.settled(), (1, 0));
        table.terminal(Terminal::wrote(live, now));
        assert_eq!(table.stale(), 2, "the second one had nothing to settle");
        assert_eq!(table.settled(), (1, 0), "and did not count as a write");
        assert_eq!(table.outstanding(), 0, "nor underflow the tracking bound");
    }

    /// A datagram no packet of which was ever accepted gives its sequence position back.
    ///
    /// The availability case the transaction exists for: without it, a client that keeps the writer's queue
    /// full can spend a hot tuple's whole cycle on attempts that never reached the wire, and the tuple is
    /// then denied its oversized output until sixty seconds after a write it is still making.
    #[test]
    fn a_datagram_nothing_accepted_gives_its_position_back() {
        let (mut table, now) = open_table(4, 8);
        let first = table.next((A, REMOTE, UDP), now).expect("issued");
        assert_eq!(first.identification(), 1);
        table.unissued(first);
        assert_eq!(table.unissued_count(), 1);

        // The same value again, because nothing anywhere carries it.
        let again = table.next((A, REMOTE, UDP), now).expect("issued");
        assert_eq!(again, first);

        // Accepted this time, so nothing asks for it back and the sequence moves on.
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

    /// Rolling back reaches only the latest issuance, so a partially accepted datagram keeps its value even
    /// if a later one gives its own back.
    #[test]
    fn rollback_reaches_only_the_latest_issuance() {
        let (mut table, now) = open_table(4, 8);
        let committed = table.next((A, REMOTE, UDP), now).expect("issued");
        table.register(committed).expect("tracked");
        let latest = table.next((A, REMOTE, UDP), now).expect("issued");
        assert_eq!(latest.identification(), 2);

        // The older one is no longer the position on top, so asking for it back changes nothing.
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

        // And a value from a tuple the table does not hold is not a rollback either.
        table.unissued(Guarded {
            tuple: (B, REMOTE, UDP),
            identification: 1,
        });
        assert_eq!(table.unissued_count(), 0);
        assert_eq!(latest.identification(), 2, "unchanged by either attempt");
    }

    /// A whole cycle of attempts that never reach the wire does not exhaust the tuple.
    #[test]
    fn attempts_that_reach_nothing_never_spend_the_cycle() {
        let (mut table, now) = open_table(4, 8);
        // One real write, so the tuple has a live window a spent cycle would have to wait out.
        let real = table.next((A, REMOTE, UDP), now).expect("issued");
        round_trip(&mut table, real, Some(now));
        for _ in 0..CYCLE + 16 {
            let attempt = table.next((A, REMOTE, UDP), now).expect("issued");
            table.unissued(attempt);
        }
        assert_eq!(table.exhausted(), 0, "not one of them was denied");
        assert_eq!(
            table
                .next((A, REMOTE, UDP), now)
                .expect("issued")
                .identification(),
            2,
            "and the sequence is still where the one real datagram left it"
        );
    }

    /// The allocator will not hand the writer a guarded packet whose ending it could not be told about.
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
        assert_eq!(table.outstanding(), 3, "and nothing was taken");

        // One settles, and the next one fits.
        table.terminal(Terminal::wrote(held[0], now));
        assert_eq!(table.outstanding(), 2);
        assert_eq!(table.register(extra), Ok(()));
        assert_eq!(table.outstanding(), 3);
    }

    /// A prepared table takes its whole bound and then refuses, and the charge is the row state that bound
    /// allows.
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
                // Each one reaches the wire, so none of them is a slot the sweep below may give away -
                // which is what makes the refusal after this a capacity refusal rather than a reclaim.
                round_trip(&mut table, guarded, Some(now));
            }
            assert_eq!(table.len(), capacity);
            // One past the bound is refused before anything is inserted. A sweep runs and finds every entry
            // too young to give away.
            assert_eq!(
                table.next((Ipv4Addr::new(203, 0, 113, 1), REMOTE, UDP), now),
                Err(Denial::AtCapacity)
            );
            assert_eq!(table.len(), capacity, "and a refusal inserts nothing");

            // The rows the bound allows, at the size of one row, plus this struct. Not the map's
            // allocation: whatever that container keeps around those rows is count-bounded rather than
            // charged, so this figure is deliberately below what the table really takes.
            let charged = Ipv4Identifications::footprint(capacity).expect("chargeable");
            assert_eq!(
                charged,
                capacity as u64 * std::mem::size_of::<(Tuple, Entry)>() as u64
                    + std::mem::size_of::<Ipv4Identifications>() as u64,
                "capacity {capacity}"
            );
        }
        // A capacity whose charge would wrap is not one that may be prepared.
        assert_eq!(Ipv4Identifications::footprint(usize::MAX), None);
    }

    /// The solved capacity is the largest one that fits, and the solver stays finite and proportional.
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
            // Proportional rather than fixed: doubling the share admits more tuples, and the table built at
            // the solved figure really takes them - `with_capacity` makes room for at least what it was asked
            // for, so its own gate is open at every count below the bound.
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
