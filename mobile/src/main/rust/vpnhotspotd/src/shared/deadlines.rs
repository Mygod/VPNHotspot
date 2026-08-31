//! Ordered deadline index for dynamically sized owner tables.
//!
//! It stores one entry per armed row, making earliest-deadline lookup and updates logarithmic while expiry
//! removes only due rows. The owning table remains authoritative; callers provide the previous deadline so
//! refresh replaces the exact index entry.
use std::collections::BTreeSet;
use std::time::Instant;

/// The armed deadlines of one table, ordered by time and then by key.
///
/// It has no independent capacity or eviction; it mirrors the owning table's armed rows.
pub struct Deadlines<K> {
    /// The key breaks equal-deadline ties.
    armed: BTreeSet<(Instant, K)>,
}

impl<K> Default for Deadlines<K> {
    fn default() -> Self {
        Self {
            armed: BTreeSet::new(),
        }
    }
}

impl<K: Ord + Copy> Deadlines<K> {
    /// Arms `key` at `deadline`, replacing its `previous` entry.
    pub fn arm(&mut self, key: K, previous: Option<Instant>, deadline: Instant) {
        if let Some(previous) = previous {
            self.armed.remove(&(previous, key));
        }
        self.armed.insert((deadline, key));
    }

    /// Removes one row's deadline.
    pub fn disarm(&mut self, key: K, deadline: Instant) {
        self.armed.remove(&(deadline, key));
    }

    /// The earliest armed deadline.
    pub fn next(&self) -> Option<Instant> {
        self.armed.first().map(|(deadline, _)| *deadline)
    }

    /// Removes and returns the earliest row when it is due.
    pub fn due(&mut self, now: Instant) -> Option<K> {
        match self.armed.first() {
            Some((deadline, _)) if *deadline <= now => self.armed.pop_first().map(|(_, key)| key),
            _ => None,
        }
    }

    /// Drops every entry.
    pub fn clear(&mut self) {
        self.armed.clear();
    }

    /// Number of armed rows.
    pub fn len(&self) -> usize {
        self.armed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.armed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn an_empty_index_has_nothing_to_wait_for_and_nothing_due() {
        let mut deadlines: Deadlines<u8> = Deadlines::default();
        assert!(deadlines.is_empty());
        assert_eq!(deadlines.next(), None);
        assert_eq!(deadlines.due(base()), None);
        assert_eq!(deadlines.len(), 0);
    }

    #[test]
    fn the_earliest_armed_row_is_the_one_waited_for() {
        let now = base();
        let mut deadlines = Deadlines::default();
        deadlines.arm(1u8, None, now + Duration::from_secs(30));
        deadlines.arm(2, None, now + Duration::from_secs(10));
        deadlines.arm(3, None, now + Duration::from_secs(20));
        assert_eq!(deadlines.next(), Some(now + Duration::from_secs(10)));
        assert_eq!(deadlines.len(), 3);
    }

    #[test]
    fn refreshing_a_row_replaces_its_entry_rather_than_adding_one() {
        let now = base();
        let mut deadlines = Deadlines::default();
        let first = now + Duration::from_secs(10);
        deadlines.arm(7u8, None, first);
        let second = now + Duration::from_secs(40);
        deadlines.arm(7, Some(first), second);
        assert_eq!(deadlines.len(), 1, "a refresh leaves no stale entry");
        assert_eq!(deadlines.next(), Some(second));
        assert_eq!(deadlines.due(now + Duration::from_secs(20)), None);
        assert_eq!(deadlines.due(second), Some(7));
        assert!(deadlines.is_empty());
    }

    #[test]
    fn a_row_removed_before_its_deadline_leaves_nothing_behind() {
        let now = base();
        let mut deadlines = Deadlines::default();
        let early = now + Duration::from_secs(5);
        let late = now + Duration::from_secs(50);
        deadlines.arm(1u8, None, early);
        deadlines.arm(2, None, late);
        deadlines.disarm(1, early);
        assert_eq!(deadlines.next(), Some(late));
        assert_eq!(deadlines.due(now + Duration::from_secs(30)), None);
        assert_eq!(deadlines.len(), 1);
    }

    #[test]
    fn only_due_rows_are_taken_and_they_come_out_in_order() {
        let now = base();
        let mut deadlines = Deadlines::default();
        for (key, seconds) in [(1u8, 30), (2, 10), (3, 20), (4, 90)] {
            deadlines.arm(key, None, now + Duration::from_secs(seconds));
        }
        let mut taken = Vec::new();
        while let Some(key) = deadlines.due(now + Duration::from_secs(30)) {
            taken.push(key);
        }
        assert_eq!(taken, vec![2, 3, 1], "staggered expiry, earliest first");
        assert_eq!(
            deadlines.next(),
            Some(now + Duration::from_secs(90)),
            "a row that is not due yet stays armed"
        );
    }

    #[test]
    fn rows_sharing_one_instant_keep_separate_entries() {
        let now = base();
        let at = now + Duration::from_secs(10);
        let mut deadlines = Deadlines::default();
        deadlines.arm(1u8, None, at);
        deadlines.arm(2, None, at);
        assert_eq!(deadlines.len(), 2);
        assert_eq!(deadlines.due(at), Some(1));
        assert_eq!(deadlines.due(at), Some(2));
        assert_eq!(deadlines.due(at), None);
    }

    #[test]
    fn clearing_releases_the_whole_index() {
        let now = base();
        let mut deadlines = Deadlines::default();
        deadlines.arm(1u8, None, now + Duration::from_secs(1));
        deadlines.arm(2, None, now + Duration::from_secs(2));
        deadlines.clear();
        assert!(deadlines.is_empty());
        assert_eq!(deadlines.next(), None);
    }
}
