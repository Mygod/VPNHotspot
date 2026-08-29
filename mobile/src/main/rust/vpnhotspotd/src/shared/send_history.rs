//! Bounded send metadata used to authenticate remote ICMP errors without retaining payloads.
use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::shared::admission::linear_footprint;

/// How long a send stays describable by an error.
const RECORD_LIFETIME: Duration = Duration::from_secs(60);

/// The payload bytes a record keeps a digest of.
const DIGEST_PREFIX: usize = 8;

/// One recorded send. Fixed-size on purpose - see the module note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Record {
    destination: SocketAddr,
    /// Of the payload's first [DIGEST_PREFIX] bytes, so an error quoting only the minimum can still be compared
    /// against it.
    digest: u64,
    /// What the client used, so a rebuilt quote carries its value rather than a substitute.
    hop_limit: u8,
    deadline: Instant,
}

/// What consulting the history came to. Every one of these ends correlated translation for the mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Exactly one record describes it, and this is the hop limit the client sent it with.
    Matched { hop_limit: u8 },
    /// More than one record describes it, so which datagram the error is about is not knowable.
    Ambiguous,
    /// No record describes it: either the daemon never sent it, or the send is already forgotten.
    Untracked,
    /// Correlated translation was already spent, or was never available.
    Spent,
}

pub struct History {
    /// Oldest first, which is both the eviction order and the expiry order, so neither needs a search.
    records: VecDeque<Record>,
    /// Once false, no further correlated translation happens for this mapping generation and the records are
    /// gone. It never becomes true again; a new generation gets a new history.
    correlating: bool,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// Prepares for `depth` records, so that recording a send inside that depth allocates nothing.
    pub fn with_capacity(depth: usize) -> Self {
        Self {
            records: VecDeque::with_capacity(depth),
            correlating: true,
        }
    }

    /// What a history prepared for `depth` records *allocates*: the deque's own backing, conservatively.
    pub fn footprint(depth: usize) -> Option<u64> {
        linear_footprint(depth, std::mem::size_of::<Record>() as u64)
    }

    /// Records one send, dropping the oldest if `depth` is reached.
    pub fn record(
        &mut self,
        destination: SocketAddr,
        payload: &[u8],
        hop_limit: u8,
        depth: usize,
        now: Instant,
    ) -> isize {
        if !self.correlating {
            return 0;
        }
        let before = self.records.len();
        self.expire(now);
        // Re-checked after expiring, because expiring is itself a reason to stop: pushing into a history that
        // has just forgotten something would build one with a hole in it.
        if !self.correlating || self.records.len() >= depth {
            self.retire();
            return -(before as isize);
        }
        self.records.push_back(Record {
            destination,
            digest: digest(payload),
            hop_limit,
            deadline: now + RECORD_LIFETIME,
        });
        self.records.len() as isize - before as isize
    }

    /// Asks whether one error's offending datagram was really sent, and ends correlation either way.
    pub fn resolve(
        &mut self,
        destination: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) -> (Resolution, usize) {
        if !self.correlating {
            return (Resolution::Spent, 0);
        }
        let before = self.records.len();
        self.expire(now);
        // A history that has forgotten anything answers [Resolution::Spent] rather than
        // [Resolution::Untracked], and the difference is not cosmetic: "I have records and yours is not among
        // them" is evidence of an error about a datagram nobody sent, while "I no longer remember" is evidence
        // of nothing at all. Reporting forgetfulness as the former would make a counter that is supposed to
        // surface forged errors climb on ordinary idle traffic.
        if !self.correlating {
            return (Resolution::Spent, before);
        }
        // The endpoint always has to match; the digest only when the quote carried payload to compute one from.
        let quoted = (!payload.is_empty()).then(|| digest(payload));
        let mut matches = self.records.iter().filter(|record| {
            record.destination == destination && quoted.is_none_or(|quoted| record.digest == quoted)
        });
        let resolution = match (matches.next(), matches.next()) {
            (Some(record), None) => Resolution::Matched {
                hop_limit: record.hop_limit,
            },
            (Some(_), Some(_)) => Resolution::Ambiguous,
            _ => Resolution::Untracked,
        };
        let released = self.records.len();
        self.retire();
        (resolution, released)
    }

    /// Drops what has aged out. Expiry is a resolution in waiting rather than a resolution itself: a history
    /// that has merely forgotten its oldest records can still match a recent datagram, so this does not retire
    /// anything on its own.
    fn expire(&mut self, now: Instant) {
        while self
            .records
            .front()
            .is_some_and(|record| record.deadline <= now)
        {
            self.records.pop_front();
            // Forgetting a send makes "never sent" and "no longer remembered" indistinguishable for everything
            // older, which is exactly the confusion correlation exists to avoid.
            self.correlating = false;
        }
        if !self.correlating {
            self.records.clear();
        }
    }

    /// Ends correlated translation for this mapping generation and releases the records.
    fn retire(&mut self) {
        self.correlating = false;
        self.records.clear();
    }

    /// How many records this holds, which is what the caller charges for.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Whether a correlated error could still be answered, so the caller can skip the work when it cannot.
    pub fn correlating(&self) -> bool {
        self.correlating
    }
}

/// Of a payload's first [DIGEST_PREFIX] bytes, which is all a conforming quote is guaranteed to return.
fn digest(payload: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    payload[..payload.len().min(DIGEST_PREFIX)].hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESTINATION: &str = "93.184.216.34:443";
    const OTHER: &str = "198.51.100.7:443";
    const DEPTH: usize = 8;

    fn destination(which: &str) -> SocketAddr {
        which.parse().unwrap()
    }

    #[test]
    fn the_charge_is_the_deque_backing_and_not_the_header_in_its_owners_row() {
        let record = std::mem::size_of::<Record>() as u64;
        for depth in [1usize, DEPTH, 64] {
            assert_eq!(
                History::footprint(depth).expect("chargeable"),
                linear_footprint(depth, record).expect("chargeable"),
                "the deque's whole backing, conservatively"
            );
        }
        assert!(
            std::mem::size_of::<History>() > 0,
            "the header exists to be charged somewhere"
        );
        assert_eq!(
            History::footprint(0),
            Some(0),
            "and it is not charged here: a history prepared for nothing allocates nothing"
        );
    }

    #[test]
    fn a_sent_datagram_is_recognised_with_the_hop_limit_it_used() {
        let mut history = History::new();
        let now = Instant::now();
        assert_eq!(
            history.record(destination(DESTINATION), b"query", 57, DEPTH, now),
            1
        );
        assert_eq!(
            history.resolve(destination(DESTINATION), b"query", now),
            (Resolution::Matched { hop_limit: 57 }, 1)
        );
    }

    #[test]
    fn one_answer_per_mapping_and_then_nothing() {
        let mut history = History::new();
        let now = Instant::now();
        history.record(destination(DESTINATION), b"query", 57, DEPTH, now);
        history.record(destination(DESTINATION), b"second", 57, DEPTH, now);
        assert!(history.correlating());
        assert!(matches!(
            history.resolve(destination(DESTINATION), b"query", now),
            (Resolution::Matched { .. }, 2)
        ));
        assert!(!history.correlating());
        assert!(history.is_empty());
        assert_eq!(
            history.resolve(destination(DESTINATION), b"second", now),
            (Resolution::Spent, 0)
        );
        assert_eq!(
            history.record(destination(DESTINATION), b"third", 57, DEPTH, now),
            0
        );
        assert!(history.is_empty());
    }

    #[test]
    fn the_wrong_destination_or_payload_is_untracked_rather_than_matched() {
        for (place, payload) in [(OTHER, &b"query"[..]), (DESTINATION, &b"other"[..])] {
            let mut history = History::new();
            let now = Instant::now();
            history.record(destination(DESTINATION), b"query", 57, DEPTH, now);
            assert_eq!(
                history.resolve(destination(place), payload, now),
                (Resolution::Untracked, 1),
                "{place} {payload:?}"
            );
        }
    }

    #[test]
    fn a_quote_carrying_no_payload_still_matches_its_endpoint() {
        let mut history = History::new();
        let now = Instant::now();
        history.record(destination(DESTINATION), b"query", 57, DEPTH, now);
        assert_eq!(
            history.resolve(destination(DESTINATION), b"", now),
            (Resolution::Matched { hop_limit: 57 }, 1)
        );
    }

    #[test]
    fn an_empty_quote_naming_a_port_never_sent_to_is_untracked() {
        let mut history = History::new();
        let now = Instant::now();
        history.record(destination(DESTINATION), b"query", 57, DEPTH, now);
        assert_eq!(
            history.resolve("93.184.216.34:9".parse().unwrap(), b"", now),
            (Resolution::Untracked, 1)
        );
    }

    #[test]
    fn only_the_quotable_prefix_decides_a_payload_match() {
        let mut history = History::new();
        let now = Instant::now();
        history.record(destination(DESTINATION), b"12345678aaaa", 57, DEPTH, now);
        history.record(destination(DESTINATION), b"12345678bbbb", 42, DEPTH, now);
        assert_eq!(
            history.resolve(destination(DESTINATION), b"12345678", now),
            (Resolution::Ambiguous, 2)
        );
    }

    #[test]
    fn two_identical_sends_cannot_be_told_apart_and_say_so() {
        let mut history = History::new();
        let now = Instant::now();
        history.record(destination(DESTINATION), b"query", 57, DEPTH, now);
        history.record(destination(DESTINATION), b"query", 42, DEPTH, now);
        assert_eq!(
            history.resolve(destination(DESTINATION), b"query", now),
            (Resolution::Ambiguous, 2)
        );
        assert!(!history.correlating());
    }

    #[test]
    fn a_full_history_retires_rather_than_forgetting_one_send() {
        let mut history = History::new();
        let now = Instant::now();
        let mut charged = 0isize;
        for i in 0..DEPTH {
            charged += history.record(destination(DESTINATION), &[i as u8], 57, DEPTH, now);
        }
        assert_eq!(charged, DEPTH as isize);
        assert!(history.correlating());
        assert_eq!(
            history.record(destination(DESTINATION), b"overflow", 57, DEPTH, now),
            -(DEPTH as isize)
        );
        assert!(!history.correlating());
        assert!(history.is_empty());
        assert_eq!(
            history.resolve(destination(DESTINATION), &[0], now),
            (Resolution::Spent, 0)
        );
    }

    #[test]
    fn a_record_that_ages_out_ends_correlation_even_if_a_newer_one_matches() {
        let mut history = History::new();
        let now = Instant::now();
        history.record(destination(DESTINATION), b"old", 57, DEPTH, now);
        let later = now + RECORD_LIFETIME;
        assert_eq!(
            history.record(destination(DESTINATION), b"new", 57, DEPTH, later),
            -1
        );
        assert!(!history.correlating());
        assert!(history.is_empty());
        assert_eq!(
            history.resolve(destination(DESTINATION), b"new", later),
            (Resolution::Spent, 0)
        );
    }

    #[test]
    fn the_deadline_is_absolute_and_later_sends_do_not_extend_it() {
        let mut history = History::new();
        let now = Instant::now();
        history.record(destination(DESTINATION), b"first", 57, DEPTH, now);
        history.record(
            destination(DESTINATION),
            b"second",
            57,
            DEPTH,
            now + Duration::from_secs(1),
        );
        assert_eq!(history.len(), 2);
        assert!(history.correlating());
        assert_eq!(
            history.resolve(destination(DESTINATION), b"second", now + RECORD_LIFETIME),
            (Resolution::Spent, 2)
        );
    }
}
