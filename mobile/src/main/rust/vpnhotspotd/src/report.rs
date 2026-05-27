use std::ffi::CString;
use std::fmt;
use std::io;
use std::io::Write;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use libc::{c_char, c_int};
use tokio::sync::mpsc::{
    unbounded_channel, UnboundedReceiver, UnboundedSender, WeakUnboundedSender,
};
use tokio::sync::oneshot;
use vpnhotspotd::shared::nonfatal::{NonfatalCoalescer, NonfatalReport};
use vpnhotspotd::shared::proto::daemon::DaemonErrorReport;
use vpnhotspotd::shared::protocol::{
    daemon_error_report, daemon_io_error_report, daemon_io_error_report_with_details,
    nonfatal_frame,
};

static REPORTER: OnceLock<UnboundedSender<ReportCommand>> = OnceLock::new();
const NONFATAL_COALESCE_WINDOW: Duration = Duration::from_secs(1);
const ANDROID_LOG_INFO: c_int = 4;
const ANDROID_LOG_ERROR: c_int = 6;
const LOG_TAG: &[u8] = b"vpnhotspotd\0";

pub(crate) type ControllerSender = UnboundedSender<ControllerMessage>;
pub(crate) type WeakControllerSender = WeakUnboundedSender<ControllerMessage>;

pub(crate) enum ControllerMessage {
    Frame(Vec<u8>),
    Nonfatal {
        frame: Vec<u8>,
        report: DaemonErrorReport,
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

    fn send_nonfatal(&self, call_id: Option<u64>, report: DaemonErrorReport) -> bool;
}

impl ControllerSenderExt for ControllerSender {
    fn send_frame(&self, frame: Vec<u8>) -> bool {
        self.send(ControllerMessage::Frame(frame)).is_ok()
    }

    fn send_nonfatal(&self, call_id: Option<u64>, report: DaemonErrorReport) -> bool {
        self.send(ControllerMessage::Nonfatal {
            frame: nonfatal_frame(call_id, report.clone()),
            report,
        })
        .is_ok()
    }
}

enum ReportCommand {
    Report {
        call_id: Option<u64>,
        report: DaemonErrorReport,
    },
    Flush {
        done: oneshot::Sender<()>,
    },
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

pub(crate) fn init(sender: ControllerSender) -> io::Result<()> {
    let controller = sender.downgrade();
    let (report_sender, report_receiver) = unbounded_channel();
    REPORTER
        .set(report_sender)
        .map_err(|_| io::Error::other("nonfatal reporter already initialized"))?;
    tokio::spawn(run_reporter(controller, report_receiver));
    Ok(())
}

pub(crate) fn report(report: DaemonErrorReport) {
    report_for(None, report);
}

pub(crate) fn report_for(call_id: Option<u64>, report: DaemonErrorReport) {
    if let Some(sender) = REPORTER.get() {
        if sender
            .send(ReportCommand::Report {
                call_id,
                report: report.clone(),
            })
            .is_ok()
        {
            return;
        }
    }
    stderr!("nonfatal report dropped after controller disconnect: {report:?}");
}

pub(crate) async fn flush() {
    let Some(sender) = REPORTER.get() else {
        return;
    };
    let (done, flushed) = oneshot::channel();
    if sender.send(ReportCommand::Flush { done }).is_ok() {
        let _ = flushed.await;
    }
}

async fn run_reporter(
    controller: WeakControllerSender,
    mut commands: UnboundedReceiver<ReportCommand>,
) {
    let mut coalescer = NonfatalCoalescer::new(NONFATAL_COALESCE_WINDOW);
    loop {
        let command = if let Some(deadline) = coalescer.next_deadline() {
            tokio::select! {
                command = commands.recv() => command,
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    emit_reports(&controller, coalescer.emit_due(Instant::now()));
                    continue;
                }
            }
        } else {
            commands.recv().await
        };
        let Some(command) = command else {
            emit_reports(&controller, coalescer.flush());
            break;
        };
        match command {
            ReportCommand::Report { call_id, report } => {
                emit_reports(&controller, coalescer.push(Instant::now(), call_id, report));
            }
            ReportCommand::Flush { done } => {
                emit_reports(&controller, coalescer.flush());
                let _ = done.send(());
            }
        }
    }
}

fn emit_reports(controller: &WeakControllerSender, reports: Vec<NonfatalReport>) {
    for NonfatalReport { call_id, report } in reports {
        if let Some(sender) = controller.upgrade() {
            if sender.send_nonfatal(call_id, report.clone()) {
                continue;
            }
        }
        stderr!("nonfatal report dropped after controller disconnect: {report:?}");
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
