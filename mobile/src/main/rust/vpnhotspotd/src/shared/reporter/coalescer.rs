//! The app session's own nonfatal coalescer.
//!
//! Deliberately not [crate::shared::nonfatal::NonfatalCoalescer], which the root daemon uses unchanged. The
//! two look alike and are not interchangeable, because the two things that define a coalescer differ:
//!
//! - **What counts as the same report.** Root keys on `(context, kind, errno, file, line)`, which groups by
//!   what went wrong. This keys on the *source site* alone - `(file, line, column)` - because the app
//!   dataplane's reports are driven by attacker-influenced packets, so the bound has to be a count the source
//!   fixes. A client that can vary `kind` or `errno` could otherwise open a batch per variation.
//! - **What happens when the writer is full.** Root's queue is unbounded and nothing waits on room. This
//!   session's writer queue is bounded, so a summary that falls due with no place to go stays *in the map*
//!   with its deadline pushed out, and later reports keep coalescing into it. Removing it and holding it
//!   elsewhere would open a fresh batch per blocked window and split one site's suppression across many
//!   summaries.
//!
//! Sharing one type would mean giving root a budget it has no use for and giving this a key it cannot be
//! bounded by, so they are two types on purpose.

use std::collections::{hash_map::Entry, HashMap};
use std::time::{Duration, Instant};

use crate::shared::nonfatal::{add_coalesced_details, NonfatalReport};
use crate::shared::proto::daemon::DaemonErrorReport;

#[derive(Debug)]
pub struct SiteCoalescer {
    window: Duration,
    pending: HashMap<SiteKey, PendingBatch>,
}

impl SiteCoalescer {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: HashMap::new(),
        }
    }

    /// Takes one report and returns whatever that made due, within `room` places in the writer's queue.
    ///
    /// A first report from a site goes out at once *if* there is room for it; without room the site opens a
    /// batch immediately instead, so the report is summarized rather than dropped and nothing is queued that
    /// the writer cannot take.
    pub fn push(
        &mut self,
        now: Instant,
        call_id: Option<u64>,
        report: DaemonErrorReport,
        room: usize,
    ) -> Vec<NonfatalReport> {
        let mut ready = self.emit_due(now, room);
        match self.pending.entry(SiteKey::from(&report)) {
            Entry::Occupied(mut entry) => {
                let batch = entry.get_mut();
                batch.suppressed_count = batch.suppressed_count.saturating_add(1);
                batch.last = Some(NonfatalReport { call_id, report });
            }
            Entry::Vacant(entry) if ready.len() < room => {
                ready.push(NonfatalReport { call_id, report });
                entry.insert(PendingBatch {
                    deadline: now + self.window,
                    suppressed_count: 0,
                    last: None,
                });
            }
            Entry::Vacant(entry) => {
                // No place for an immediate one, so this report *is* the batch: counted from one, and carried
                // as the summary's last report until there is room to hand it over.
                entry.insert(PendingBatch {
                    deadline: now + self.window,
                    suppressed_count: 1,
                    last: Some(NonfatalReport { call_id, report }),
                });
            }
        }
        ready
    }

    /// Emits the summaries that have fallen due, up to `room` of them.
    ///
    /// A batch that is due but has no place keeps everything it has - its count, its last report and its slot
    /// in the map - so the next report from that site still coalesces into it. That is the whole difference
    /// from the root coalescer, and it is what makes one blocked window cost one summary rather than many.
    pub fn emit_due(&mut self, now: Instant, room: usize) -> Vec<NonfatalReport> {
        let due_keys = self
            .pending
            .iter()
            .filter_map(|(key, batch)| (batch.deadline <= now).then_some(key.clone()))
            .collect::<Vec<_>>();
        let mut ready = Vec::new();
        for key in due_keys {
            let mut remove = false;
            if let Some(batch) = self.pending.get_mut(&key) {
                if batch.suppressed_count == 0 {
                    remove = true;
                } else if ready.len() >= room {
                    // Retained rather than emitted or dropped: the deadline moves so this is reconsidered on
                    // the next pass, and the count keeps accumulating in the meantime.
                    batch.deadline = now + self.window;
                } else if let Some(mut report) = batch.last.take() {
                    add_coalesced_details(&mut report.report, batch.suppressed_count, self.window);
                    ready.push(report);
                    batch.suppressed_count = 0;
                    batch.deadline = now + self.window;
                } else {
                    remove = true;
                }
            }
            if remove {
                self.pending.remove(&key);
            }
        }
        ready
    }

    /// Everything still held, at the end of the conversation. Unbounded by room on purpose: the finalizer
    /// hands these over one at a time and waits for the writer between them.
    pub fn flush(&mut self) -> Vec<NonfatalReport> {
        let mut ready = Vec::new();
        for mut batch in self.pending.drain().map(|(_, batch)| batch) {
            if batch.suppressed_count > 0 {
                if let Some(mut report) = batch.last.take() {
                    add_coalesced_details(&mut report.report, batch.suppressed_count, self.window);
                    ready.push(report);
                }
            }
        }
        ready
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|batch| batch.deadline).min()
    }
}

#[derive(Debug)]
struct PendingBatch {
    deadline: Instant,
    suppressed_count: usize,
    last: Option<NonfatalReport>,
}

/// One `report!` call site, and nothing about what it said.
///
/// The bound this buys is a count the *source* fixes: however a client varies the traffic, the number of
/// distinct sites in this binary does not change.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SiteKey {
    file: String,
    line: u32,
    column: u32,
}

impl From<&DaemonErrorReport> for SiteKey {
    fn from(report: &DaemonErrorReport) -> Self {
        Self {
            file: report.file.clone(),
            line: report.line,
            column: report.column,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::nonfatal::SUPPRESSED_COUNT_DETAIL;

    fn report(context: &str, kind: &str, line: u32, column: u32) -> DaemonErrorReport {
        DaemonErrorReport {
            context: context.to_owned(),
            message: "message".to_owned(),
            kind: kind.to_owned(),
            errno: None,
            file: "src/app.rs".to_owned(),
            line,
            column,
            pid: 0,
            details: Vec::new(),
        }
    }

    fn suppressed(report: &NonfatalReport) -> Option<&str> {
        report
            .report
            .details
            .iter()
            .find(|detail| detail.key == SUPPRESSED_COUNT_DETAIL)
            .map(|detail| detail.value.as_str())
    }

    const ROOM: usize = usize::MAX;

    /// The source site alone decides identity: two reports from one `report!` coalesce however differently a
    /// client makes them describe themselves.
    ///
    /// This is the property root's key deliberately does not have, and the reason this type exists. Root
    /// groups by what went wrong, which is right for a daemon whose reports it authors; here the *content* is
    /// attacker-influenced, so grouping by it would let one site become as many batches as a client can think
    /// of variations.
    #[test]
    fn one_site_is_one_batch_however_the_report_varies() {
        let mut coalescer = SiteCoalescer::new(Duration::from_secs(1));
        let now = Instant::now();
        assert_eq!(
            coalescer
                .push(now, None, report("a", "One", 7, 3), ROOM)
                .len(),
            1
        );
        // Same site, different context, kind and call id - still the same batch.
        assert!(coalescer
            .push(now, Some(9), report("b", "Two", 7, 3), ROOM)
            .is_empty());
        assert!(coalescer
            .push(now, None, report("c", "Three", 7, 3), ROOM)
            .is_empty());
        let due = coalescer.emit_due(now + Duration::from_secs(2), ROOM);
        assert_eq!(due.len(), 1, "one site, one summary");
        assert_eq!(suppressed(&due[0]), Some("2"));

        // A different column is a different call site, so it is a batch of its own.
        assert_eq!(
            coalescer
                .push(now, None, report("a", "One", 7, 40), ROOM)
                .len(),
            1
        );
    }

    /// A due batch with nowhere to go stays in the map and keeps accumulating.
    ///
    /// The whole reason this is not root's coalescer. Removing the batch - emitting it, or buffering it
    /// outside - would open a fresh one for the next report and split one site's suppression across as many
    /// summaries as there were blocked windows.
    #[test]
    fn a_blocked_batch_is_retained_and_keeps_counting() {
        let window = Duration::from_secs(1);
        let mut coalescer = SiteCoalescer::new(window);
        let mut now = Instant::now();
        // No room at all, so even the first report opens a batch instead of going out.
        assert!(coalescer
            .push(now, None, report("a", "One", 7, 3), 0)
            .is_empty());
        for _ in 0..5u32 {
            now += window * 2;
            for _ in 0..10u32 {
                assert!(coalescer
                    .push(now, None, report("a", "One", 7, 3), 0)
                    .is_empty());
            }
            // Due every time, and every time retained rather than dropped.
            assert!(coalescer.emit_due(now, 0).is_empty());
        }
        // One summary, carrying every occurrence: 1 opening + 50 coalesced.
        let due = coalescer.emit_due(now + window * 2, ROOM);
        assert_eq!(due.len(), 1);
        assert_eq!(suppressed(&due[0]), Some("51"));
    }

    /// The final flush is not bounded by room: the finalizer hands summaries over one at a time itself.
    #[test]
    fn flush_yields_what_a_blocked_window_kept() {
        let mut coalescer = SiteCoalescer::new(Duration::from_secs(1));
        let now = Instant::now();
        assert!(coalescer
            .push(now, None, report("a", "One", 7, 3), 0)
            .is_empty());
        assert!(coalescer
            .push(now, None, report("a", "One", 9, 3), 0)
            .is_empty());
        assert_eq!(coalescer.flush().len(), 2);
        assert!(coalescer.next_deadline().is_none());
    }
}
