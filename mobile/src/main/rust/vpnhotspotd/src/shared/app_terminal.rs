//! Selects the session's single terminal error without losing independently observed failures.
use std::io;

use crate::shared::proto::daemon::DaemonErrorReport;
use crate::shared::protocol::{complete_frame, error_frame};
use crate::shared::reporter::ReporterGuard;

/// Which call is still owed this session's one terminal frame, and what that frame will carry.
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
    Refused {
        call_id: u64,
        report: DaemonErrorReport,
    },
}

impl Terminal {
    /// Routes one independently-owned failure: taken when the frame this session owes is still looking for
    /// something to carry, and handed straight back when it is not.
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
    const HANDOFF: usize = 1;
    const CONFIG_CALL: u64 = 12;
    const SESSION_CALL: u64 = 1;

    struct Queued {
        frame: Vec<u8>,
        _place: Option<Handed>,
    }

    #[derive(Debug)]
    enum Wrote {
        Frame(Frame),
        Closed,
    }

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
            }
            wrote.push(Wrote::Closed);
            wrote
        })
    }

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

    #[tokio::test]
    async fn every_report_reaches_the_queue_before_a_refused_config_is_answered() {
        let (control, queue) = unbounded_channel();
        let writing = spawn_writer(queue);
        let registry = ReporterRegistry::new();
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
        let unanswered = terminal
            .claim(report("shizuku.tun_output", 77))
            .expect("a refused config leaves a task's failure to the reporter");
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
        drop(control);

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
        assert_eq!(report_lines(reports), [42, 42, 77]);
        let Wrote::Frame(Frame::Error(error)) = answer else {
            panic!("a refused config is answered with an error frame, not {answer:?}");
        };
        assert_eq!(error.call_id, CONFIG_CALL);
        assert_eq!(error.report, Some(refusal()));
    }

    #[tokio::test]
    async fn the_start_call_is_answered_once_and_a_quiet_ending_not_at_all() {
        let mut terminal = Terminal::Start {
            call_id: SESSION_CALL,
            report: None,
        };
        assert!(terminal.claim(report("shizuku.app_session", 5)).is_none());
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
