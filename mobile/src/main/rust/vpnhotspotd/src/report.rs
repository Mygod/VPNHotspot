use std::ffi::CString;
use std::fmt;
use std::io;
use std::io::Write;
use std::sync::LazyLock;

use libc::{c_char, c_int};
use tokio::sync::mpsc::UnboundedSender;
use vpnhotspotd::shared::nonfatal::NonfatalReport;
use vpnhotspotd::shared::proto::daemon::DaemonErrorReport;
use vpnhotspotd::shared::protocol::{
    daemon_error_report, daemon_error_report_with_details, daemon_io_error_report,
    daemon_io_error_report_with_details, nonfatal_frame,
};
use vpnhotspotd::shared::reporter::{Handed, Pushed, Reporter, ReporterGuard, ReporterRegistry};

/// Process-wide registry for the active control conversation.
static REPORTER: LazyLock<ReporterRegistry> = LazyLock::new(ReporterRegistry::default);

/// Limits nonfatals in the unbounded serial writer queue to one. Extra slots cannot increase throughput;
/// while this one is occupied the coalescer retains only the latest report per compiled source site.
const NONFATAL_QUEUE: usize = 1;
const ANDROID_LOG_INFO: c_int = 4;
const ANDROID_LOG_ERROR: c_int = 6;
const LOG_TAG: &[u8] = b"vpnhotspotd\0";

pub(crate) type ControllerSender = UnboundedSender<ControllerMessage>;

pub(crate) enum ControllerMessage {
    Frame(Vec<u8>),
    Nonfatal {
        frame: Vec<u8>,
        report: DaemonErrorReport,
        /// Releases the reporter's handoff slot when the writer drops this message.
        _place: Handed,
    },
}

impl ControllerMessage {
    pub(crate) fn packet(&self) -> &[u8] {
        match self {
            Self::Frame(frame) | Self::Nonfatal { frame, .. } => frame,
        }
    }

    pub(crate) fn log_send_failure(self, error: io::Error) {
        match self {
            Self::Frame(_) => write_stderr(format_args!("controller send failed: {error}")),
            Self::Nonfatal { report, .. } => {
                write_stderr(format_args!(
                    "nonfatal report dropped after controller send failed: {error}: {report:?}"
                ));
            }
        }
    }

    pub(crate) fn log_drop_after_disconnect(self) {
        if let Self::Nonfatal { report, .. } = self {
            write_stderr(format_args!(
                "nonfatal report dropped after controller disconnect: {report:?}"
            ));
        }
    }
}

pub(crate) trait ControllerSenderExt {
    fn send_frame(&self, frame: Vec<u8>) -> bool;
}

impl ControllerSenderExt for ControllerSender {
    fn send_frame(&self, frame: Vec<u8>) -> bool {
        self.send(ControllerMessage::Frame(frame)).is_ok()
    }
}

macro_rules! stdout {
    ($($arg:tt)*) => {
        $crate::report::write_stdout(format_args!($($arg)*))
    };
}

macro_rules! stderr {
    ($($arg:tt)*) => {
        $crate::report::write_stderr(format_args!($($arg)*))
    };
}

pub(crate) use stderr;
pub(crate) use stdout;

#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(priority: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

/// Installs this conversation's reporter. The weak sender cannot extend the writer's lifetime.
pub(crate) fn init(sender: ControllerSender) -> io::Result<ReporterGuard> {
    let controller = sender.downgrade();
    REPORTER.install(Reporter::new(
        NONFATAL_QUEUE,
        move |NonfatalReport { call_id, report }, place| match controller.upgrade() {
            Some(sender) => {
                match sender.send(ControllerMessage::Nonfatal {
                    frame: nonfatal_frame(call_id, report.clone()),
                    report,
                    _place: place,
                }) {
                    Ok(()) => true,
                    Err(e) => {
                        e.0.log_drop_after_disconnect();
                        false
                    }
                }
            }
            None => {
                stderr!("nonfatal report dropped after controller disconnect: {report:?}");
                false
            }
        },
    ))
}

pub(crate) fn report(report: DaemonErrorReport) {
    report_for(None, report);
}

/// Routes a report to the active conversation, or logs it when none can accept it.
pub(crate) fn report_for(call_id: Option<u64>, report: DaemonErrorReport) {
    let Some(reporter) = REPORTER.get() else {
        stderr!("nonfatal report made with no reporter to carry it: {report:?}");
        return;
    };
    match reporter.push(call_id, report) {
        Pushed::Coalesced => {}
        Pushed::Closed(report) => {
            stderr!("nonfatal report made after the reporter finished: {report:?}");
        }
    }
}

#[track_caller]
pub(crate) fn io(context: impl Into<String>, error: io::Error) {
    report(daemon_io_error_report(context, error));
}

#[track_caller]
pub(crate) fn message(
    context: impl Into<String>,
    message: impl Into<String>,
    kind: impl Into<String>,
) {
    report(daemon_error_report(context, message, kind));
}

#[track_caller]
pub(crate) fn message_with_details<I, K, V>(
    context: impl Into<String>,
    message: impl Into<String>,
    kind: impl Into<String>,
    details: I,
) where
    I: IntoIterator<Item = (K, V)>,
    K: ToString,
    V: ToString,
{
    report(daemon_error_report_with_details(
        context, message, kind, details,
    ));
}

/// Keeps the first of two independently observed failures, and reports the other.
///
/// The counterpart to [vpnhotspotd::shared::tasks::combine], and the distinction is whose failure each is.
/// That folds two halves of a *single* ending - a session that failed and could not shut down cleanly - into
/// one message. This is for failures with separate causes, where that fold would be a lie and, worse, a loss:
/// [vpnhotspotd::shared::tasks::combine] builds a fresh error out of two messages, so the structured report
/// each failing step attached survives neither.
///
/// One result carries one error, so only one of these can travel out on it. The first observed stays the
/// causal failure its owner ends on; the second becomes a nonfatal here, because dropping it is exactly the
/// silent discard structured reporting exists to prevent.
///
/// `context` is only used for a failure that arrived with no report of its own:
/// [vpnhotspotd::shared::protocol::describe_io_error] hands an attached one back unchanged, so what is
/// emitted names the failing site rather than this one.
#[track_caller]
pub(crate) fn keep_first(
    context: &'static str,
    kept: io::Result<()>,
    beside: io::Result<()>,
) -> io::Result<()> {
    let Err(kept) = kept else {
        // Nothing has been observed yet, so whatever this is becomes the causal failure - or stays `Ok`.
        return beside;
    };
    if let Err(beside) = beside {
        io_with_details(context, beside, std::iter::empty::<(&str, &str)>());
    }
    Err(kept)
}

#[track_caller]
pub(crate) fn io_with_details<I, K, V>(context: impl Into<String>, error: io::Error, details: I)
where
    I: IntoIterator<Item = (K, V)>,
    K: ToString,
    V: ToString,
{
    report(daemon_io_error_report_with_details(context, error, details));
}

pub(crate) fn write_stdout(message: fmt::Arguments<'_>) {
    write_stdio(io::stdout().lock(), ANDROID_LOG_INFO, message);
}

pub(crate) fn write_stderr(message: fmt::Arguments<'_>) {
    write_stdio(io::stderr().lock(), ANDROID_LOG_ERROR, message);
}

fn write_stdio(mut writer: impl Write, priority: c_int, message: fmt::Arguments<'_>) {
    let message = message.to_string();
    if writer.write_fmt(format_args!("{message}\n")).is_err() {
        write_logcat(priority, &message);
    }
}

fn write_logcat(priority: c_int, message: &str) {
    let mut bytes = message.as_bytes().to_vec();
    for byte in &mut bytes {
        if *byte == 0 {
            *byte = b' ';
        }
    }
    let Ok(message) = CString::new(bytes) else {
        return;
    };
    unsafe {
        __android_log_write(priority, LOG_TAG.as_ptr().cast(), message.as_ptr());
    }
}
