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

#[derive(Debug)]
pub struct NonfatalCoalescer {
    window: Duration,
    pending: HashMap<ReportKey, PendingBatch>,
}

impl NonfatalCoalescer {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: HashMap::new(),
        }
    }

    pub fn push(
        &mut self,
        now: Instant,
        call_id: Option<u64>,
        report: DaemonErrorReport,
    ) -> Vec<NonfatalReport> {
        let mut ready = self.emit_due(now);
        match self.pending.entry(ReportKey::from(&report)) {
            Entry::Occupied(mut entry) => {
                let batch = entry.get_mut();
                batch.suppressed_count = batch.suppressed_count.saturating_add(1);
                batch.last = Some(NonfatalReport { call_id, report });
            }
            Entry::Vacant(entry) => {
                ready.push(NonfatalReport { call_id, report });
                entry.insert(PendingBatch {
                    deadline: now + self.window,
                    suppressed_count: 0,
                    last: None,
                });
            }
        }
        ready
    }

    pub fn emit_due(&mut self, now: Instant) -> Vec<NonfatalReport> {
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReportKey {
    context: String,
    kind: String,
    errno: Option<i32>,
    file: String,
    line: u32,
}

impl From<&DaemonErrorReport> for ReportKey {
    fn from(report: &DaemonErrorReport) -> Self {
        Self {
            context: report.context.clone(),
            kind: report.kind.clone(),
            errno: report.errno,
            file: report.file.clone(),
            line: report.line,
        }
    }
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
        let mut coalescer = NonfatalCoalescer::new(window);

        let ready = coalescer.push(now, Some(1), report("dns.counter", "Other", "first", 10));
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].call_id, Some(1));
        assert_eq!(ready[0].report.message, "first");

        assert!(coalescer
            .push(
                now + Duration::from_millis(100),
                Some(2),
                report("dns.counter", "Other", "last", 10),
            )
            .is_empty());

        let ready = coalescer.emit_due(now + window);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].call_id, Some(2));
        assert_eq!(ready[0].report.message, "last");
        assert_summary(&ready[0].report, 1, 1000);
    }

    #[test]
    fn continuous_reports_emit_one_summary_per_window_without_new_immediate_report() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = NonfatalCoalescer::new(window);

        assert_eq!(
            coalescer
                .push(now, None, report("nat66.udp_recv", "Other", "first", 20))
                .len(),
            1,
        );
        assert!(coalescer
            .push(
                now + Duration::from_millis(200),
                None,
                report("nat66.udp_recv", "Other", "second", 20),
            )
            .is_empty());

        let ready = coalescer.emit_due(now + window);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "second");
        assert_summary(&ready[0].report, 1, 1000);

        assert!(coalescer
            .push(
                now + window + Duration::from_millis(200),
                None,
                report("nat66.udp_recv", "Other", "third", 20),
            )
            .is_empty());

        let ready = coalescer.emit_due(now + window + window);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "third");
        assert_summary(&ready[0].report, 1, 1000);
    }

    #[test]
    fn quiet_batch_closes_and_next_report_is_immediate() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = NonfatalCoalescer::new(window);

        assert_eq!(
            coalescer
                .push(now, None, report("routing.apply", "Other", "first", 30))
                .len(),
            1,
        );
        assert!(coalescer.emit_due(now + window).is_empty());

        let ready = coalescer.push(
            now + window + Duration::from_millis(1),
            None,
            report("routing.apply", "Other", "second", 30),
        );
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "second");
    }

    #[test]
    fn flush_emits_pending_summary() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = NonfatalCoalescer::new(window);

        assert_eq!(
            coalescer
                .push(
                    now,
                    None,
                    report("control.call_join", "JoinError", "first", 40)
                )
                .len(),
            1,
        );
        assert!(coalescer
            .push(
                now + Duration::from_millis(100),
                None,
                report("control.call_join", "JoinError", "last", 40),
            )
            .is_empty());

        let ready = coalescer.flush();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "last");
        assert_summary(&ready[0].report, 1, 1000);
        assert!(coalescer.flush().is_empty());
    }

    #[test]
    fn different_report_keys_do_not_coalesce() {
        let window = Duration::from_secs(1);
        let now = Instant::now();
        let mut coalescer = NonfatalCoalescer::new(window);

        assert_eq!(
            coalescer
                .push(now, None, report("dns.counter", "Other", "first", 50))
                .len(),
            1,
        );
        let ready = coalescer.push(
            now + Duration::from_millis(100),
            None,
            report("dns.counter", "Other", "second", 51),
        );
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "second");
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
