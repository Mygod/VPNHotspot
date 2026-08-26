//! Which call is owed the app-UID session's one terminal frame, and what has to have happened before that
//! frame is written.
//!
//! Here rather than beside the session because none of it is Android's: it is which call ID is still owed a
//! frame, which report that frame carries, and the order the frame is produced in. The session owns the
//! writer's queue, the dataplane and the descriptors; this owns the decision and the ordering.
//!
//! The ordering is why this is a type rather than a call ID beside a flag. The app reads this conversation
//! with a single reader, and that reader returns on the first terminal frame it takes off the queue - a
//! terminal frame *is* how the session ends, whichever call it names. So every structured report the session
//! still owes has to be on the writer's FIFO before that frame, and nothing may follow it. A refused config
//! used to answer its own call the moment it was refused, which put its `ErrorFrame` ahead of everything
//! teardown had not raised yet - the reports still inside a coalescing window, and the dataplane failures the
//! join was about to discover - and the app never saw any of them. [Terminal::answer] is that ordering,
//! expressed so a caller cannot take the halves the other way round: there is no frame until the flush that
//! must precede it has completed.

use std::io;

use crate::shared::proto::daemon::DaemonErrorReport;
use crate::shared::protocol::{complete_frame, error_frame};
use crate::shared::reporter::ReporterGuard;

/// Which call is still owed this session's one terminal frame, and what that frame will carry.
///
/// One value rather than a call ID beside an "already answered" flag, because these three are all the states
/// there are: the pairs such a flag could also form - answered while a call is still owed a frame, unanswered
/// with no call to answer on - are not any of them.
pub enum Terminal {
    /// Nobody is owed anything. Either no frame has named a call yet, or the ending leaves nobody waiting:
    /// control-socket EOF and a cancel both remove the call on the app's side before the daemon notices.
    Nobody,
    /// The start call, which owns the session for as long as it runs and is owed whichever terminal the
    /// ending turns out to be.
    Start {
        call_id: u64,
        /// What the frame will carry, once a failure has claimed it - see [Terminal::claim]. Still `None` at
        /// the end is a session that ended cleanly with the app connected, which is answered with a
        /// completion instead.
        report: Option<DaemonErrorReport>,
    },
    /// A config call that was refused, whose `ErrorFrame` is this session's terminal frame.
    ///
    /// The report is the one built at the check that refused the config, so its context, errno, details and
    /// Rust source location are that site's rather than the teardown's. Nothing else may claim this frame:
    /// the failure is answered on the call that caused it, and the start call must not receive a second frame
    /// carrying the same thing.
    Refused {
        call_id: u64,
        report: DaemonErrorReport,
    },
}

impl Terminal {
    /// Routes one independently-owned failure: taken when the frame this session owes is still looking for
    /// something to carry, and handed straight back when it is not.
    ///
    /// Handed back rather than dropped, because a failure nothing is going to answer for still has to reach
    /// the app - as a structured nonfatal - and the caller is what owns reporting. Mirrors
    /// [crate::shared::reporter::Pushed::Closed] for the same reason.
    ///
    /// At most one failure is ever taken. The first one offered wins, which is the caller's ordering rather
    /// than this one's: the session offers its own failure before its dataplane's, because a task that ended
    /// when the session did is the consequence rather than the cause.
    pub fn claim(&mut self, report: DaemonErrorReport) -> Option<DaemonErrorReport> {
        match self {
            Self::Start {
                report: carried, ..
            } if carried.is_none() => {
                *carried = Some(report);
                None
            }
            _ => Some(report),
        }
    }

    /// Ends this conversation's reporting and only then answers the call that is owed a frame.
    ///
    /// Both halves are here because the order between them is the contract, and a caller holding the two
    /// separately is exactly how it was got wrong. The reporter's flush hands its last summaries to the same
    /// writer this frame goes to, so finishing it first is what puts them ahead of the frame that makes the
    /// app stop reading; the frame does not exist until that flush has returned, so nothing can write it
    /// early. Dropping the last sender afterwards is the caller's, and is what turns "nothing follows the
    /// terminal frame" into the EOF the app reads.
    ///
    /// `reporter` is `None` on the paths where installing one never happened - a start call refused before it
    /// had a call ID to belong to - and those have nothing to flush.
    ///
    /// `send` is handed the call ID beside the frame because the caller is what reports a frame it could no
    /// longer write, and that report has to name the call left unanswered.
    ///
    /// Returns the reporter's own outcome, which is the session's to fold in: an undelivered report is a
    /// failure of this session rather than of whatever produced it.
    pub async fn answer(
        self,
        reporter: Option<ReporterGuard>,
        send: impl FnOnce(u64, Vec<u8>),
    ) -> io::Result<()> {
        let reported = match reporter {
            Some(reporter) => reporter.finish().await,
            None => Ok(()),
        };
        match self {
            Self::Nobody => {}
            Self::Start { call_id, report } => send(
                call_id,
                match report {
                    Some(report) => error_frame(call_id, report),
                    // A clean ending with the app still connected, which is the dataplane having finished on
                    // its own: the call is over rather than broken, and the app is told which.
                    None => complete_frame(call_id),
                },
            ),
            Self::Refused { call_id, report } => send(call_id, error_frame(call_id, report)),
        }
        reported
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use prost::Message;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
    use tokio::task::JoinHandle;

    use super::*;
    use crate::shared::nonfatal::NonfatalReport;
    use crate::shared::proto::daemon::{daemon_envelope::Frame, DaemonEnvelope, ErrorDetail};
    use crate::shared::protocol::nonfatal_frame;
    use crate::shared::reporter::{Handed, Pushed, Reporter, ReporterRegistry};

    const WINDOW: Duration = Duration::from_secs(1);
    /// The app session's own queue depth, so the handoff below is bounded exactly as it is in production.
    const HANDOFF: usize = 1;
    const CONFIG_CALL: u64 = 12;
    const SESSION_CALL: u64 = 1;

    /// One entry on the modelled control writer's queue.
    ///
    /// Shaped like the daemon's own `ControllerMessage`, because the shape *is* the property under test: a
    /// nonfatal carries the place in the queue it spends, and that place is given back by *dropping* the
    /// entry. So the reporter cannot hand over its next report until the writer below has taken this one off
    /// the queue, recorded it and released it - which is what makes the flush inside [Terminal::answer] wait
    /// on real writer progress rather than on nothing.
    struct Queued {
        frame: Vec<u8>,
        /// `None` for the session's own frames, which spend no place: only the reporter's share of the queue
        /// is bounded. Named for nothing to read, as in production - it is released by dropping the entry.
        _place: Option<Handed>,
    }

    /// What the modelled writer saw, in the order it saw it.
    ///
    /// [Wrote::Closed] is recorded rather than inferred, so "the terminal frame is the last one the session
    /// writes, and the stream then ends" is a position in this sequence instead of something a test could
    /// forget to check. It exists only because `recv` returned `None`, which is every sender having been
    /// dropped.
    #[derive(Debug)]
    enum Wrote {
        Frame(Frame),
        Closed,
    }

    /// The control writer, modelled: one task taking one entry off the queue at a time, in queue order.
    ///
    /// A concurrent task rather than a drain at the end, and `recv` rather than `try_recv`, because both
    /// halves of the property need it. Concurrent, so the reporter's final handoff really waits for this
    /// writer to give its one place back; blocking on `recv`, so what ends the recording is the queue
    /// *closing* rather than the queue merely being empty at the moment somebody looked - an entry still on
    /// its way, or a sender still holding the queue open, parks here instead of passing.
    fn spawn_writer(mut queue: UnboundedReceiver<Queued>) -> JoinHandle<Vec<Wrote>> {
        tokio::spawn(async move {
            let mut wrote = Vec::new();
            while let Some(Queued { frame, _place }) = queue.recv().await {
                wrote.push(Wrote::Frame(
                    DaemonEnvelope::decode(frame.as_slice())
                        .expect("the daemon writes what it encoded")
                        .frame
                        .expect("every daemon frame carries one"),
                ));
                // `_place` is dropped here, after the frame has been recorded, exactly as the daemon's writer
                // releases a place by dropping the message it has finished writing.
            }
            wrote.push(Wrote::Closed);
            wrote
        })
    }

    /// Reports are keyed by source site, so a caller that wants two distinct batches varies the line.
    fn report(context: &str, line: u32) -> DaemonErrorReport {
        DaemonErrorReport {
            context: context.to_owned(),
            message: "a mapping's receive failed".to_owned(),
            errno: Some(libc::EIO),
            kind: "Other".to_owned(),
            file: "src/shizuku/udp.rs".to_owned(),
            line,
            column: 1,
            pid: 321,
            details: Vec::new(),
        }
    }

    /// The refusal's own report, built where the config was refused: an errno, details and a source location
    /// the teardown does not have and must not replace.
    fn refusal() -> DaemonErrorReport {
        DaemonErrorReport {
            context: "shizuku.control.config".to_owned(),
            message: "config 4 is not a valid successor".to_owned(),
            errno: Some(libc::EINVAL),
            kind: "InvalidData".to_owned(),
            file: "src/shizuku/app_config.rs".to_owned(),
            line: 171,
            column: 24,
            pid: 321,
            details: vec![ErrorDetail {
                key: "call_id".to_owned(),
                value: CONFIG_CALL.to_string(),
            }],
        }
    }

    /// The source line of each report the writer recorded, so what reached the app can be checked without
    /// depending on the order the coalescer's map drains its batches in.
    fn report_lines(wrote: &[Wrote]) -> Vec<u32> {
        let mut lines = wrote
            .iter()
            .map(|frame| match frame {
                Wrote::Frame(Frame::NonFatal(nonfatal)) => {
                    nonfatal
                        .report
                        .as_ref()
                        .expect("every nonfatal frame carries a report")
                        .line
                }
                other => panic!("only reports precede the terminal frame, not {other:?}"),
            })
            .collect::<Vec<_>>();
        lines.sort_unstable();
        lines
    }

    /// The bug this ordering exists for. A refused config is terminal, and the app's reader returns on the
    /// `ErrorFrame` that answers it - so a report still owed when that frame is written is a report the app
    /// never sees. Both kinds are owed here: one already coalesced into a window before teardown began, and
    /// one raised by teardown itself.
    #[tokio::test]
    async fn every_report_reaches_the_queue_before_a_refused_config_is_answered() {
        // One queue and one serial writer taking frames off it: the reporter's emissions go on it and so does
        // the terminal frame, so what the writer records is the order the app reads.
        let (control, queue) = unbounded_channel();
        let writing = spawn_writer(queue);
        let registry = ReporterRegistry::new();
        // Downgraded exactly as the app session's own reporter installation does, so reporting cannot hold the
        // queue open past the point its owner drops the last sender - which is what the stream ending means.
        let sink = control.downgrade();
        let reporter = registry
            .install(Reporter::new(
                WINDOW,
                HANDOFF,
                move |NonfatalReport { call_id, report }, place| match sink.upgrade() {
                    Some(control) => control
                        .send(Queued {
                            frame: nonfatal_frame(call_id, report),
                            _place: Some(place),
                        })
                        .is_ok(),
                    None => false,
                },
            ))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        // Two from one site: the first is emitted at once and the second is held in its window, which is the
        // report a terminal frame written at refusal time loses.
        for _ in 0..2 {
            assert!(matches!(
                pushing.push(None, report("shizuku.udp_send", 42)),
                Pushed::Coalesced
            ));
        }
        let mut terminal = Terminal::Refused {
            call_id: CONFIG_CALL,
            report: refusal(),
        };
        // Teardown-discovered: a dataplane half whose failure the join found while the session was already
        // ending. The refusal is answered on its own call, so this has no call to belong to and the terminal
        // hands it straight back to be reported.
        let unanswered = terminal
            .claim(report("shizuku.tun_output", 77))
            .expect("a refused config leaves a task's failure to the reporter");
        // Whether this one goes out at once or as its own summary depends on whether the writer has given the
        // place back yet, and neither is the property: both reach the queue before the terminal frame.
        assert!(matches!(pushing.push(None, unanswered), Pushed::Coalesced));
        drop(pushing);

        assert!(terminal
            .answer(Some(reporter), |call_id, frame| {
                assert_eq!(call_id, CONFIG_CALL);
                assert!(control
                    .send(Queued {
                        frame,
                        _place: None
                    })
                    .is_ok());
            })
            .await
            .is_ok());
        // What the session does next, and what makes the stream end after the terminal frame rather than
        // stay open behind it.
        drop(control);

        // Awaited rather than drained: this returns only once the writer has seen the queue close, so a frame
        // still on its way would be recorded rather than missed.
        let wrote = writing
            .await
            .expect("the writer records the queue to its close");
        let (closed, written) = wrote
            .split_last()
            .expect("the writer records at least the close");
        assert!(
            matches!(closed, Wrote::Closed),
            "nothing follows the terminal frame, so the queue closes next, not {closed:?}"
        );
        let (answer, reports) = written
            .split_last()
            .expect("the refusal is answered on its own call");
        // Every report first, and none of them lost: the site pushed twice appears as its immediate report and
        // again as the summary the flush released, and the teardown-discovered one as its own.
        assert_eq!(report_lines(reports), [42, 42, 77]);
        // Then the terminal frame, carrying the report the refusing site built rather than a rebuilt one.
        let Wrote::Frame(Frame::Error(error)) = answer else {
            panic!("a refused config is answered with an error frame, not {answer:?}");
        };
        assert_eq!(error.call_id, CONFIG_CALL);
        assert_eq!(error.report, Some(refusal()));
    }

    /// The other two endings, and the one thing that must not happen on either: the start call is owed at
    /// most one frame, and a failure that cannot claim it is the reporter's rather than nobody's.
    #[tokio::test]
    async fn the_start_call_is_answered_once_and_a_quiet_ending_not_at_all() {
        let mut terminal = Terminal::Start {
            call_id: SESSION_CALL,
            report: None,
        };
        assert!(terminal.claim(report("shizuku.app_session", 5)).is_none());
        // A second failure finds the frame taken, so it goes out as a nonfatal instead of replacing the
        // failure the session is about.
        assert!(terminal.claim(report("shizuku.tun_egress", 6)).is_some());

        let (control, queue) = unbounded_channel();
        let writing = spawn_writer(queue);
        assert!(terminal
            .answer(None, |_, frame| assert!(control
                .send(Queued {
                    frame,
                    _place: None
                })
                .is_ok()))
            .await
            .is_ok());
        // A clean ending is a completion rather than an error, and the app is told which.
        assert!(Terminal::Start {
            call_id: SESSION_CALL,
            report: None,
        }
        .answer(None, |_, frame| assert!(control
            .send(Queued {
                frame,
                _place: None
            })
            .is_ok()))
        .await
        .is_ok());
        // Nobody is waiting after control-socket EOF or a cancel, so nothing is written at all.
        assert!(Terminal::Nobody
            .answer(None, |_, _| panic!("a quiet ending answers nothing"))
            .await
            .is_ok());
        drop(control);

        let wrote = writing
            .await
            .expect("the writer records the queue to its close");
        assert_eq!(wrote.len(), 3);
        let Wrote::Frame(Frame::Error(error)) = &wrote[0] else {
            panic!(
                "a session that failed is answered with an error frame, not {:?}",
                wrote[0]
            );
        };
        assert_eq!(error.call_id, SESSION_CALL);
        assert_eq!(error.report, Some(report("shizuku.app_session", 5)));
        assert!(
            matches!(wrote[1], Wrote::Frame(Frame::Complete(_))),
            "{:?}",
            wrote[1]
        );
        assert!(matches!(wrote[2], Wrote::Closed), "{:?}", wrote[2]);
    }
}
