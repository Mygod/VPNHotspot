use std::collections::{hash_map::Entry, HashMap, VecDeque};

use crate::shared::proto::daemon::{DaemonErrorReport, ErrorDetail};

const SUPPRESSED_COUNT_DETAIL: &str = "coalesced.suppressed_count";

#[derive(Debug, Clone)]
pub struct NonfatalReport {
    pub call_id: Option<u64>,
    pub report: DaemonErrorReport,
}

/// Coalesces by compiled source site, bounding pending batches independently of report payload.
#[derive(Debug)]
pub(crate) struct SiteCoalescer {
    pending: HashMap<SiteKey, Pending>,
    /// First-blocked order, kept separately because a hash table deliberately promises none.
    order: VecDeque<SiteKey>,
}

impl SiteCoalescer {
    pub(crate) fn new() -> Self {
        Self {
            pending: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub(crate) fn push(
        &mut self,
        call_id: Option<u64>,
        report: DaemonErrorReport,
        room: usize,
    ) -> (Vec<NonfatalReport>, bool) {
        // A place that returned belongs to the oldest blocked source before a newer report may take it.
        let was_empty = self.pending.is_empty();
        let mut ready = self.emit(room);
        match self.pending.entry(SiteKey {
            file: report.file.clone(),
            line: report.line,
            column: report.column,
        }) {
            Entry::Occupied(mut entry) => {
                let batch = entry.get_mut();
                batch.suppressed_count = batch.suppressed_count.saturating_add(1);
                batch.last = NonfatalReport { call_id, report };
            }
            Entry::Vacant(_) if ready.len() < room => {
                ready.push(NonfatalReport { call_id, report });
            }
            Entry::Vacant(entry) => {
                self.order.push_back(entry.key().clone());
                entry.insert(Pending {
                    suppressed_count: 0,
                    last: NonfatalReport { call_id, report },
                });
            }
        }
        (ready, was_empty && !self.pending.is_empty())
    }

    /// Emits the oldest blocked sources that fit. A source has exactly one pending report: its latest.
    pub(crate) fn emit(&mut self, room: usize) -> Vec<NonfatalReport> {
        let mut ready = Vec::with_capacity(room.min(self.pending.len()));
        while ready.len() < room {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            let mut batch = self
                .pending
                .remove(&key)
                .expect("a pending report has its place in the order");
            if batch.suppressed_count > 0 {
                add_coalesced_detail(&mut batch.last.report, batch.suppressed_count);
            }
            ready.push(batch.last);
        }
        debug_assert_eq!(self.order.is_empty(), self.pending.is_empty());
        ready
    }

    pub(crate) fn flush(&mut self) -> Vec<NonfatalReport> {
        self.emit(usize::MAX)
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[derive(Debug)]
struct Pending {
    /// Reports this pending one replaces, not the latest report that will still be delivered.
    suppressed_count: usize,
    last: NonfatalReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SiteKey {
    file: String,
    line: u32,
    column: u32,
}

fn add_coalesced_detail(report: &mut DaemonErrorReport, suppressed_count: usize) {
    report.details.push(ErrorDetail {
        key: SUPPRESSED_COUNT_DETAIL.to_owned(),
        value: suppressed_count.to_string(),
    });
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
    fn every_report_is_immediate_while_the_handoff_has_room() {
        let mut coalescer = SiteCoalescer::new();
        for (call_id, message) in [(1, "first"), (2, "second")] {
            let (ready, opened) = coalescer.push(
                Some(call_id),
                report("dns.counter", "Other", message, 10),
                1,
            );
            assert!(!opened, "an immediate report leaves no pending batch");
            assert_eq!(ready.len(), 1);
            assert_eq!(ready[0].call_id, Some(call_id));
            assert_eq!(ready[0].report.message, message);
            assert_eq!(summary(&ready[0].report), None);
        }
    }

    #[test]
    fn a_blocked_source_retains_its_latest_report_and_counts_only_replacements() {
        let mut coalescer = SiteCoalescer::new();
        let (ready, opened) =
            coalescer.push(Some(1), report("dns.counter", "Other", "first", 10), 0);
        assert!(ready.is_empty());
        assert!(opened);
        for (call_id, message) in [(2, "second"), (3, "last")] {
            let (ready, opened) = coalescer.push(
                Some(call_id),
                report("dns.counter", "Other", message, 10),
                0,
            );
            assert!(ready.is_empty());
            assert!(!opened);
        }

        let ready = coalescer.emit(1);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].call_id, Some(3));
        assert_eq!(ready[0].report.message, "last");
        assert_eq!(summary(&ready[0].report), Some("2"));
    }

    #[test]
    fn a_returned_place_goes_to_the_oldest_waiter_before_a_new_report() {
        let mut coalescer = SiteCoalescer::new();
        assert!(coalescer
            .push(None, report("one", "A", "first", 10), 0)
            .0
            .is_empty());

        let (ready, opened) = coalescer.push(None, report("two", "B", "second", 20), 1);
        assert!(!opened, "the drain worker already owns the pending state");
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "first");
        assert_eq!(summary(&ready[0].report), None);

        let ready = coalescer.emit(1);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "second");
    }

    #[test]
    fn flush_preserves_first_blocked_order_and_runs_once() {
        let mut coalescer = SiteCoalescer::new();
        for (line, message) in [(30, "first"), (20, "second"), (30, "latest"), (40, "last")] {
            assert!(coalescer
                .push(
                    None,
                    report("control.call_join", "JoinError", message, line),
                    0
                )
                .0
                .is_empty());
        }

        let ready = coalescer.flush();
        assert_eq!(
            ready
                .iter()
                .map(|report| (report.report.line, report.report.message.as_str()))
                .collect::<Vec<_>>(),
            [(30, "latest"), (20, "second"), (40, "last")]
        );
        assert_eq!(summary(&ready[0].report), Some("1"));
        assert_eq!(summary(&ready[1].report), None);
        assert_eq!(summary(&ready[2].report), None);
        assert!(coalescer.flush().is_empty());
    }

    #[test]
    fn one_source_site_is_one_category_whatever_the_reports_carry() {
        let mut coalescer = SiteCoalescer::new();
        coalescer.push(None, report("nat66.udp_recv", "Other", "first", 60), 0);
        let mut unrelated = report("dns.counter", "BrokenPipe", "second", 60);
        unrelated.errno = Some(libc::EPIPE);
        unrelated.details = vec![ErrorDetail {
            key: "client".to_owned(),
            value: "02:00:00:00:00:01".to_owned(),
        }];
        coalescer.push(Some(9), unrelated, 0);

        let ready = coalescer.emit(1);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].call_id, Some(9));
        assert_eq!(ready[0].report.context, "dns.counter");
        assert_eq!(ready[0].report.kind, "BrokenPipe");
        assert_eq!(ready[0].report.errno, Some(libc::EPIPE));
        assert_eq!(summary(&ready[0].report), Some("1"));
    }

    #[test]
    fn file_line_and_column_each_separate_source_sites() {
        let mut coalescer = SiteCoalescer::new();
        coalescer.push(None, report("one", "A", "first", 50), 0);
        coalescer.push(None, report("two", "B", "second", 51), 0);
        let mut other_column = report("three", "C", "third", 51);
        other_column.column = 2;
        coalescer.push(None, other_column, 0);
        let mut other_file = report("four", "D", "fourth", 51);
        other_file.file = "src/other.rs".to_owned();
        coalescer.push(None, other_file, 0);

        assert_eq!(
            coalescer
                .flush()
                .iter()
                .map(|report| report.report.message.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third", "fourth"]
        );
    }

    #[test]
    fn zero_room_leaves_a_single_report_intact_until_room_returns() {
        let mut coalescer = SiteCoalescer::new();
        coalescer.push(None, report("one", "A", "first", 7), 0);
        assert!(coalescer.emit(0).is_empty());
        assert!(coalescer.has_pending());

        let ready = coalescer.emit(1);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].report.message, "first");
        assert_eq!(summary(&ready[0].report), None);
        assert!(!coalescer.has_pending());
    }

    #[test]
    fn a_summary_keeps_every_detail_from_the_latest_report() {
        let mut coalescer = SiteCoalescer::new();
        coalescer.push(None, report("one", "A", "first", 7), 0);
        let mut latest = report("one", "A", "latest", 7);
        latest.details = (0..12)
            .map(|index| ErrorDetail {
                key: format!("key-{index}"),
                value: format!("value-{index}"),
            })
            .collect();
        coalescer.push(None, latest, 0);

        let ready = coalescer.emit(1);
        assert_eq!(ready[0].report.details.len(), 13);
        for index in 0..12 {
            assert_eq!(ready[0].report.details[index].key, format!("key-{index}"));
            assert_eq!(
                ready[0].report.details[index].value,
                format!("value-{index}")
            );
        }
        assert_eq!(summary(&ready[0].report), Some("1"));
        assert!(ready[0]
            .report
            .details
            .iter()
            .all(|detail| detail.key != "coalesced.window_ms"));
    }

    fn summary(report: &DaemonErrorReport) -> Option<&str> {
        report
            .details
            .iter()
            .find(|detail| detail.key == SUPPRESSED_COUNT_DETAIL)
            .map(|detail| detail.value.as_str())
    }
}
