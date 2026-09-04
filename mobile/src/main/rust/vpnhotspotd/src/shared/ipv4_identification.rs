//! Per-tuple IPv4 Identification issue, wire settlement and reuse quarantine.
//!
//! Identifications are issued only for fragmented IPv4 output. A tuple cannot reuse values until all writes
//! settle and [MDL] passes. Tuple rows are dynamic and uncapped.
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// Assumed maximum datagram lifetime for Identification reuse.
///
/// **Derivation:** [RFC 6864 section
/// 5.2](https://www.rfc-editor.org/rfc/rfc6864.html#section-5.2) describes 120 seconds as a typical maximum;
/// [RFC 1122 section 3.3.2](https://www.rfc-editor.org/rfc/rfc1122.html#page-58) gives a 60–120 second
/// reassembly range. The daemon takes the conservative upper endpoint.
///
/// **Failure mode:** reuse inside a receiver's reassembly window can combine different datagrams.
///
/// **Exhaustion:** when the field is exhausted, oversized IPv4 output is dropped until all settlements arrive
/// and [MDL] passes after the last fragment write. Atomic IPv4 and IPv6 are unaffected.
pub const MDL: Duration = Duration::from_secs(120);

/// Values issued before a tuple must wait for reuse.
const CYCLE: u32 = 1 << 16;

/// What a receiver reassembles on, minus the Identification this hands out.
pub type Tuple = (Ipv4Addr, Ipv4Addr, u8);

/// One guarded datagram's identity, issued before it is built and carried by every fragment it becomes.
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

/// What the TUN writer says about one guarded logical datagram it took ownership of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terminal {
    guarded: Guarded,
    /// Last successful fragment write, or `None` if none reached the wire.
    written: Option<Instant>,
}

impl Terminal {
    /// At least one fragment reached the wire, last at `at`.
    pub fn wrote(guarded: Guarded, at: Instant) -> Self {
        Self {
            guarded,
            written: Some(at),
        }
    }

    /// The writer owned the datagram and no fragment of it reached the wire.
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

/// Why one oversized IPv4 datagram could not be given an Identification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// The session cannot know a predecessor's still-live wire Identifications.
    Quarantined,
    /// This tuple cannot start another cycle yet.
    Exhausted,
}

/// One tuple's sequential issue and settlement state, without a per-value bitmap.
struct Entry {
    /// How many of this cycle's Identifications have been issued. [CYCLE] means the cycle is spent.
    issued: u32,
    /// Accepted datagrams whose wire outcome is not yet known.
    pending: u32,
    /// Latest successful fragment write.
    written: Option<Instant>,
}

impl Entry {
    /// Whether this tuple's sequence may start again.
    fn reusable(&self, now: Instant) -> bool {
        self.pending == 0
            && self
                .written
                .is_none_or(|at| now.saturating_duration_since(at) >= MDL)
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

/// The one allocator, owned by the dataplane task and shared by every producer.
pub struct Ipv4Identifications {
    entries: HashMap<Tuple, Entry>,
    /// When this session's allocator opened, and so when its opening quarantine started.
    opened: Instant,
    exhausted: u64,
    quarantined: u64,
    /// Issues returned before any fragment could reach the wire.
    unissued: u64,
    accepted: u64,
    written: u64,
    unwritten: u64,
    /// Settlements with no matching pending datagram.
    stale: u64,
}

impl Ipv4Identifications {
    pub fn new(opened: Instant) -> Self {
        Self {
            entries: HashMap::new(),
            opened,
            exhausted: 0,
            quarantined: 0,
            unissued: 0,
            accepted: 0,
            written: 0,
            unwritten: 0,
            stale: 0,
        }
    }

    /// The next Identification for this tuple, or the reason there is not one.
    pub fn next(&mut self, tuple: Tuple, now: Instant) -> Result<Guarded, Denial> {
        // No tuple is safe until predecessor wire state has aged out.
        if now.saturating_duration_since(self.opened) < MDL {
            self.quarantined += 1;
            return Err(Denial::Quarantined);
        }
        let issued = self
            .entries
            .entry(tuple)
            .or_insert(Entry {
                issued: 0,
                pending: 0,
                written: None,
            })
            .issue(now);
        match issued {
            Some(identification) => Ok(Guarded {
                tuple,
                identification,
            }),
            None => {
                self.exhausted += 1;
                Err(Denial::Exhausted)
            }
        }
    }

    /// Returns the latest issue when the writer accepted no fragment.
    pub fn unissued(&mut self, guarded: Guarded) {
        if self
            .entries
            .get_mut(&guarded.tuple)
            .is_some_and(|entry| entry.unissue(guarded.identification))
        {
            self.unissued += 1;
        }
    }

    /// Registers one accepted guarded datagram.
    pub fn accepted(&mut self, guarded: Guarded) {
        match self.entries.get_mut(&guarded.tuple) {
            Some(entry) => {
                entry.pending += 1;
                self.accepted += 1;
            }
            // Issue and registration are synchronous.
            None => self.stale += 1,
        }
    }

    /// Applies one ending the writer sent back.
    pub fn terminal(&mut self, terminal: Terminal) {
        let Terminal { guarded, written } = terminal;
        let settled = match self.entries.get_mut(&guarded.tuple) {
            Some(entry) if entry.pending > 0 => {
                entry.pending -= 1;
                if let Some(at) = written {
                    // Settlement order may differ from write order.
                    if entry.written.is_none_or(|previous| previous < at) {
                        entry.written = Some(at);
                    }
                }
                true
            }
            _ => false,
        };
        if !settled {
            self.stale += 1;
            return;
        }
        match written {
            Some(_) => self.written += 1,
            None => self.unwritten += 1,
        }
    }

    /// Datagrams denied because their tuple had spent its sequence and its window had not passed.
    pub fn exhausted(&self) -> u64 {
        self.exhausted
    }

    /// Datagrams denied because the session had not been open for [MDL] yet.
    pub fn quarantined(&self) -> u64 {
        self.quarantined
    }

    /// Guarded datagrams the writer has accepted and not yet settled.
    pub fn outstanding(&self) -> u64 {
        self.accepted - self.written - self.unwritten
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn describe(&self) -> String {
        format!(
            "ipv4-identification tuples {} quarantined {} exhausted {} unissued {} accepted {} \
             written {} unwritten {} outstanding {} stale {}",
            self.len(),
            self.quarantined,
            self.exhausted,
            self.unissued,
            self.accepted,
            self.written,
            self.unwritten,
            self.outstanding(),
            self.stale
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const A: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
    const B: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
    const REMOTE: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 1);

    /// A table after opening quarantine.
    fn opened() -> (Ipv4Identifications, Instant) {
        let opened = Instant::now();
        (Ipv4Identifications::new(opened), opened + MDL)
    }

    #[test]
    fn the_opening_quarantine_refuses_every_tuple_until_a_whole_lifetime_has_passed() {
        let opened = Instant::now();
        let mut identifications = Ipv4Identifications::new(opened);
        let tuple = (A, REMOTE, 17);
        assert_eq!(
            identifications.next(tuple, opened),
            Err(Denial::Quarantined)
        );
        assert_eq!(
            identifications.next(tuple, opened + MDL - Duration::from_millis(1)),
            Err(Denial::Quarantined)
        );
        assert_eq!(
            identifications.next((B, REMOTE, 6), opened),
            Err(Denial::Quarantined)
        );
        assert_eq!(identifications.quarantined(), 3);
        assert!(
            identifications.is_empty(),
            "a quarantined request creates no row"
        );
        assert!(identifications.next(tuple, opened + MDL).is_ok());
    }

    #[test]
    fn a_tuple_issues_every_one_of_the_sixty_five_thousand_values_exactly_once() {
        let (mut identifications, now) = opened();
        let tuple = (A, REMOTE, 17);
        let mut seen = HashSet::with_capacity(CYCLE as usize);
        for issue in 0..CYCLE {
            let guarded = identifications
                .next(tuple, now)
                .unwrap_or_else(|denial| panic!("issue {issue} denied: {denial:?}"));
            assert_eq!(guarded.tuple(), tuple);
            assert!(
                seen.insert(guarded.identification()),
                "issue {issue} repeated {}",
                guarded.identification()
            );
            identifications.accepted(guarded);
            identifications.terminal(Terminal::wrote(guarded, now));
        }
        assert_eq!(seen.len(), CYCLE as usize, "the complete 16-bit space");
        assert_eq!(
            identifications.next(tuple, now),
            Err(Denial::Exhausted),
            "and then it is spent"
        );
        assert_eq!(identifications.exhausted(), 1);
    }

    #[test]
    fn a_spent_tuple_waits_for_settlement_and_then_for_the_whole_lifetime() {
        let (mut identifications, now) = opened();
        let tuple = (A, REMOTE, 17);
        let mut last = None;
        for _ in 0..CYCLE {
            last = Some(identifications.next(tuple, now).expect("a fresh cycle"));
        }
        let last = last.expect("the cycle issued something");
        identifications.accepted(last);
        assert_eq!(identifications.outstanding(), 1);

        // Elapsed time cannot replace a missing settlement.
        assert_eq!(
            identifications.next(tuple, now + MDL * 10),
            Err(Denial::Exhausted)
        );

        let wrote = now + Duration::from_secs(1);
        identifications.terminal(Terminal::wrote(last, wrote));
        assert_eq!(identifications.outstanding(), 0);
        assert_eq!(
            identifications.next(tuple, wrote + MDL - Duration::from_millis(1)),
            Err(Denial::Exhausted),
            "settled, but still inside the lifetime of the last fragment written"
        );
        assert_eq!(
            identifications
                .next(tuple, wrote + MDL)
                .expect("the window has passed")
                .identification(),
            1,
            "and the new cycle starts at the beginning of the space"
        );
    }

    #[test]
    fn a_tuple_that_never_reached_the_wire_may_start_again_at_once() {
        let (mut identifications, now) = opened();
        let tuple = (A, REMOTE, 17);
        let mut last = None;
        for _ in 0..CYCLE {
            last = Some(identifications.next(tuple, now).expect("a fresh cycle"));
        }
        let last = last.expect("the cycle issued something");
        identifications.accepted(last);
        identifications.terminal(Terminal::unwritten(last));
        assert_eq!(identifications.outstanding(), 0);
        assert!(
            identifications.next(tuple, now).is_ok(),
            "nothing of this tuple is on the wire, so nothing can collide"
        );
    }

    #[test]
    fn the_latest_write_owns_the_window_whichever_order_terminals_arrive_in() {
        let (mut identifications, now) = opened();
        let tuple = (A, REMOTE, 17);
        let mut issued = Vec::new();
        for _ in 0..CYCLE {
            issued.push(identifications.next(tuple, now).expect("a fresh cycle"));
        }
        let early = issued[0];
        let late = issued[CYCLE as usize - 1];
        identifications.accepted(early);
        identifications.accepted(late);
        let later = now + Duration::from_secs(30);
        // Settle out of order; retain the later write time.
        identifications.terminal(Terminal::wrote(late, later));
        identifications.terminal(Terminal::wrote(early, now));
        assert_eq!(
            identifications.next(tuple, now + MDL),
            Err(Denial::Exhausted)
        );
        assert!(identifications.next(tuple, later + MDL).is_ok());
    }

    #[test]
    fn a_datagram_the_writer_never_took_gives_its_position_back() {
        let (mut identifications, now) = opened();
        let tuple = (A, REMOTE, 17);
        let first = identifications.next(tuple, now).expect("the first value");
        let second = identifications.next(tuple, now).expect("the second value");
        identifications.unissued(second);
        assert_eq!(
            identifications
                .next(tuple, now)
                .expect("the position came back")
                .identification(),
            second.identification(),
            "the value nothing carried is issued again"
        );
        // Returning an older issue would repeat subsequent values.
        identifications.unissued(first);
        assert_ne!(
            identifications
                .next(tuple, now)
                .expect("the sequence carries on")
                .identification(),
            first.identification()
        );
    }

    #[test]
    fn a_partially_written_datagram_counts_as_used() {
        let (mut identifications, now) = opened();
        let tuple = (A, REMOTE, 17);
        let guarded = identifications.next(tuple, now).expect("the first value");
        identifications.accepted(guarded);
        // A partial write still exposes the Identification.
        let wrote = now + Duration::from_secs(2);
        identifications.terminal(Terminal::wrote(guarded, wrote));
        assert_eq!(
            identifications
                .next(tuple, wrote)
                .expect("the sequence carries on")
                .identification(),
            guarded.identification().wrapping_add(1),
            "a written value is never reissued inside its cycle"
        );
        for _ in 2..CYCLE {
            identifications
                .next(tuple, wrote)
                .expect("the rest of the cycle");
        }
        assert_eq!(
            identifications.next(tuple, wrote + MDL - Duration::from_millis(1)),
            Err(Denial::Exhausted),
            "and the partial write is what the reuse window is measured from"
        );
        assert!(identifications.next(tuple, wrote + MDL).is_ok());
    }

    #[test]
    fn tuples_are_independent() {
        let (mut identifications, now) = opened();
        let a = (A, REMOTE, 17);
        let b = (B, REMOTE, 17);
        let protocol = (A, REMOTE, 6);
        for _ in 0..CYCLE {
            let guarded = identifications.next(a, now).expect("a fresh cycle");
            identifications.accepted(guarded);
            identifications.terminal(Terminal::wrote(guarded, now));
        }
        assert_eq!(identifications.next(a, now), Err(Denial::Exhausted));
        assert_eq!(
            identifications
                .next(b, now)
                .expect("a fresh tuple")
                .identification(),
            1
        );
        assert_eq!(
            identifications
                .next(protocol, now)
                .expect("the protocol is part of the tuple")
                .identification(),
            1
        );
        assert_eq!(identifications.len(), 3);
    }

    #[test]
    fn a_settlement_for_nothing_registered_is_counted_rather_than_applied() {
        let (mut identifications, now) = opened();
        let tuple = (A, REMOTE, 17);
        let guarded = identifications.next(tuple, now).expect("the first value");
        identifications.terminal(Terminal::wrote(guarded, now));
        assert_eq!(identifications.outstanding(), 0);
        assert!(identifications.describe().contains("stale 1"));
    }

    #[test]
    fn describe_reports_the_dynamic_row_count_and_every_outcome() {
        let (mut identifications, now) = opened();
        assert!(identifications.is_empty());
        let tuple = (A, REMOTE, 17);
        let guarded = identifications.next(tuple, now).expect("the first value");
        identifications.accepted(guarded);
        assert!(identifications.describe().contains("tuples 1"));
        assert!(identifications.describe().contains("outstanding 1"));
        identifications.terminal(Terminal::wrote(guarded, now));
        assert!(identifications.describe().contains("written 1"));
        assert!(identifications.describe().contains("outstanding 0"));
    }
}
