use std::collections::{hash_map::Entry, HashMap};
use std::time::{Duration, Instant};

use crate::shared::proto::daemon::{DaemonErrorReport, ErrorDetail};
use crate::shared::protocol::MAX_ERROR_DETAILS;

const SUPPRESSED_COUNT_DETAIL: &str = "coalesced.suppressed_count";
const WINDOW_MS_DETAIL: &str = "coalesced.window_ms";

#[derive(Debug, Clone)]
pub struct NonfatalReport {
    pub call_id: Option<u64>,
    pub report: DaemonErrorReport,
}

/// Coalesces by compiled source site, bounding pending batches independently of report payload.
#[derive(Debug)]
pub(crate) struct SiteCoalescer {
    window: Duration,
    pending: HashMap<SiteKey, PendingBatch>,
}

impl SiteCoalescer {
    pub(crate) fn new(window: Duration) -> Self {
        Self {
            window,
            pending: HashMap::new(),
        }
    }

    pub(crate) fn push(
        &mut self,
        now: Instant,
        call_id: Option<u64>,
        report: DaemonErrorReport,
        room: usize,
    ) -> (Vec<NonfatalReport>, bool) {
        // Existing batches already provide an earlier deadline or wait for handoff room.
        let opened = self.pending.is_empty();
        let mut ready = self.emit_due(now, room);
        match self.pending.entry(SiteKey {
            file: report.file.clone(),
            line: report.line,
            column: report.column,
        }) {
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
                entry.insert(PendingBatch {
                    deadline: now + self.window,
                    suppressed_count: 1,
                    last: Some(NonfatalReport { call_id, report }),
                });
            }
        }
        (ready, opened)
    }

    /// Emits the oldest due batches that fit. With no room it avoids the global scan; skipped batches stay
    /// overdue so handoff release, rather than another window, retries them.
    pub(crate) fn emit_due(&mut self, now: Instant, room: usize) -> Vec<NonfatalReport> {
        if room == 0 {
            return Vec::new();
        }
        let mut due_keys = self
            .pending
            .iter()
            .filter(|(_, batch)| batch.deadline <= now)
            .map(|(key, batch)| (batch.deadline, key.clone()))
            .collect::<Vec<_>>();
        due_keys.sort_unstable_by_key(|(deadline, _)| *deadline);
        let mut ready = Vec::new();
        for (_, key) in due_keys {
            let mut remove = false;
            if let Some(batch) = self.pending.get_mut(&key) {
                if batch.suppressed_count == 0 {
                    remove = true;
                } else if ready.len() < room {
                    if let Some(mut report) = batch.last.take() {
                        add_coalesced_details(
                            &mut report.report,
                            batch.suppressed_count,
                            self.window,
                        );
                        ready.push(report);
                        batch.suppressed_count = 0;
                        batch.deadline = now + self.window;
                    } else {
                        remove = true;
                    }
                }
            }
            if remove {
                self.pending.remove(&key);
            }
        }
        ready
    }

    pub(crate) fn flush(&mut self) -> Vec<NonfatalReport> {
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

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|batch| batch.deadline).min()
    }
}

#[derive(Debug)]
struct PendingBatch {
    deadline: Instant,
    suppressed_count: usize,
    last: Option<NonfatalReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SiteKey {
    file: String,
    line: u32,
    column: u32,
}

fn add_coalesced_details(
    report: &mut DaemonErrorReport,
    suppressed_count: usize,
    window: Duration,
) {
    let summary_details = [
        ErrorDetail {
            key: SUPPRESSED_COUNT_DETAIL.to_owned(),
            value: suppressed_count.to_string(),
        },
        ErrorDetail {
            key: WINDOW_MS_DETAIL.to_owned(),
            value: window.as_millis().to_string(),
        },
    ];
    report
        .details
        .truncate(MAX_ERROR_DETAILS.saturating_sub(summary_details.len()));
    report.details.extend(summary_details);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(context: &str, kind: &str, message: &str, line: u32) -> DaemonErrorReport {
        DaemonErrorReport {
            context: context.to_owned(),
            message: message.to_owned(),
            errno: Some(5),
            kind: kind.to_owned(),
            file: "src/test.rs".to_owned(),
            line,
            column: 1,
            pid: 123,
            details: Vec::new(),
        }
    }

    #[test]
    fn first_report_is_immediate_and_repeat_is_summarized_with_last_report() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = SiteCoalescer::new(window);

        let (ready, opened) = coalescer.push(
            now,
            Some(1),
            report("dns.counter", "Other", "first", 10),
            usize::MAX,
        );
        assert!(opened);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].call_id, Some(1));
        assert_eq!(ready[0].report.message, "first");

        assert!(coalescer
            .push(
                now + Duration::from_millis(100),
                Some(2),
                report("dns.counter", "Other", "last", 10),
                usize::MAX,
            )
            .0
            .is_empty());

        let ready = coalescer.emit_due(now + window, usize::MAX);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].call_id, Some(2));
        assert_eq!(ready[0].report.message, "last");
        assert_summary(&ready[0].report, 1, 1000);
    }

    #[test]
    fn continuous_reports_emit_one_summary_per_window_without_new_immediate_report() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = SiteCoalescer::new(window);

        assert_eq!(
            coalescer
                .push(
                    now,
                    None,
                    report("nat66.udp_recv", "Other", "first", 20),
                    usize::MAX,
                )
                .0
                .len(),
            1,
        );
        assert!(coalescer
            .push(
                now + Duration::from_millis(200),
                None,
                report("nat66.udp_recv", "Other", "second", 20),
                usize::MAX,
            )
            .0
            .is_empty());

        let ready = coalescer.emit_due(now + window, usize::MAX);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "second");
        assert_summary(&ready[0].report, 1, 1000);

        assert!(coalescer
            .push(
                now + window + Duration::from_millis(200),
                None,
                report("nat66.udp_recv", "Other", "third", 20),
                usize::MAX,
            )
            .0
            .is_empty());

        let ready = coalescer.emit_due(now + window + window, usize::MAX);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "third");
        assert_summary(&ready[0].report, 1, 1000);
    }

    #[test]
    fn quiet_batch_closes_and_next_report_is_immediate() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = SiteCoalescer::new(window);

        assert_eq!(
            coalescer
                .push(
                    now,
                    None,
                    report("routing.apply", "Other", "first", 30),
                    usize::MAX,
                )
                .0
                .len(),
            1,
        );
        assert!(coalescer.emit_due(now + window, usize::MAX).is_empty());

        let (ready, opened) = coalescer.push(
            now + window + Duration::from_millis(1),
            None,
            report("routing.apply", "Other", "second", 30),
            usize::MAX,
        );
        assert!(opened);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "second");
    }

    #[test]
    fn flush_emits_pending_summary() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = SiteCoalescer::new(window);

        assert_eq!(
            coalescer
                .push(
                    now,
                    None,
                    report("control.call_join", "JoinError", "first", 40),
                    usize::MAX,
                )
                .0
                .len(),
            1,
        );
        assert!(coalescer
            .push(
                now + Duration::from_millis(100),
                None,
                report("control.call_join", "JoinError", "last", 40),
                usize::MAX,
            )
            .0
            .is_empty());

        let ready = coalescer.flush();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "last");
        assert_summary(&ready[0].report, 1, 1000);
        assert!(coalescer.flush().is_empty());
    }

    #[test]
    fn different_source_sites_do_not_coalesce() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = SiteCoalescer::new(window);

        assert_eq!(
            coalescer
                .push(
                    now,
                    None,
                    report("dns.counter", "Other", "first", 50),
                    usize::MAX,
                )
                .0
                .len(),
            1,
        );
        let (ready, _) = coalescer.push(
            now + Duration::from_millis(100),
            None,
            report("dns.counter", "Other", "second", 51),
            usize::MAX,
        );
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "second");

        let mut same_line_other_column = report("dns.counter", "Other", "third", 51);
        same_line_other_column.column = 2;
        let (ready, _) = coalescer.push(
            now + Duration::from_millis(200),
            None,
            same_line_other_column,
            usize::MAX,
        );
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "third");

        let mut other_file = report("dns.counter", "Other", "fourth", 51);
        other_file.file = "src/other.rs".to_owned();
        let (ready, _) = coalescer.push(
            now + Duration::from_millis(300),
            None,
            other_file,
            usize::MAX,
        );
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "fourth");
    }

    #[test]
    fn one_source_site_is_one_category_whatever_the_reports_carry() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = SiteCoalescer::new(window);

        assert_eq!(
            coalescer
                .push(
                    now,
                    None,
                    report("nat66.udp_recv", "Other", "first", 60),
                    usize::MAX,
                )
                .0
                .len(),
            1,
        );
        let mut unrelated = report("dns.counter", "BrokenPipe", "second", 60);
        unrelated.errno = Some(libc::EPIPE);
        unrelated.details = vec![ErrorDetail {
            key: "client".to_owned(),
            value: "02:00:00:00:00:01".to_owned(),
        }];
        assert!(coalescer
            .push(
                now + Duration::from_millis(100),
                Some(9),
                unrelated,
                usize::MAX,
            )
            .0
            .is_empty());

        let ready = coalescer.emit_due(now + window, usize::MAX);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].call_id, Some(9));
        assert_eq!(ready[0].report.context, "dns.counter");
        assert_eq!(ready[0].report.kind, "BrokenPipe");
        assert_eq!(ready[0].report.errno, Some(libc::EPIPE));
        assert_summary(&ready[0].report, 1, 1000);
    }

    #[test]
    fn reports_are_retained_until_the_writer_has_room() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = SiteCoalescer::new(window);
        assert!(coalescer
            .push(now, None, report("one", "A", "first", 7), 0)
            .0
            .is_empty());
        assert!(coalescer
            .push(now, None, report("two", "B", "second", 7), 0)
            .0
            .is_empty());

        let ready = coalescer.emit_due(now + window, 1);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "second");
        assert_summary(&ready[0].report, 2, 1000);

        let mut different_column = report("three", "C", "third", 7);
        different_column.column = 2;
        assert_eq!(
            coalescer
                .push(now + window, None, different_column, usize::MAX)
                .0
                .len(),
            1
        );
    }

    #[test]
    fn zero_room_leaves_an_overdue_window_untouched() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = SiteCoalescer::new(window);
        let (ready, opened) = coalescer.push(now, None, report("one", "A", "first", 7), 0);
        assert!(ready.is_empty());
        assert!(opened);
        let deadline = coalescer.next_deadline().expect("the window is open");

        assert!(coalescer.emit_due(now + window, 0).is_empty());
        assert_eq!(coalescer.next_deadline(), Some(deadline));

        let ready = coalescer.emit_due(now + window, 1);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "first");
        assert_summary(&ready[0].report, 1, 1000);
    }

    #[test]
    fn a_window_that_closed_with_no_room_keeps_the_deadline_it_had() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = SiteCoalescer::new(window);

        assert_eq!(
            coalescer
                .push(now, None, report("dns.counter", "Other", "first", 70), 1)
                .0
                .len(),
            1,
        );
        let deadline = coalescer.next_deadline().expect("the window is open");

        let (ready, opened) = coalescer.push(
            now + window,
            None,
            report("dns.counter", "Other", "second", 70),
            0,
        );
        assert!(ready.is_empty());
        assert!(!opened, "the window task already has this deadline");
        assert_eq!(coalescer.next_deadline(), Some(deadline));

        let ready = coalescer.emit_due(now + window, 1);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "second");
        assert_summary(&ready[0].report, 1, 1000);
        assert!(coalescer.next_deadline().is_some());
    }

    fn assert_summary(report: &DaemonErrorReport, suppressed_count: usize, window_ms: u128) {
        let suppressed_count = suppressed_count.to_string();
        let window_ms = window_ms.to_string();
        assert_eq!(
            detail_value(report, SUPPRESSED_COUNT_DETAIL),
            Some(suppressed_count.as_str()),
        );
        assert_eq!(
            detail_value(report, WINDOW_MS_DETAIL),
            Some(window_ms.as_str()),
        );
    }

    fn detail_value<'a>(report: &'a DaemonErrorReport, key: &str) -> Option<&'a str> {
        report
            .details
            .iter()
            .find(|detail| detail.key == key)
            .map(|detail| detail.value.as_str())
    }
}
