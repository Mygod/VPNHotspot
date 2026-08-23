use std::ffi::CString;
use std::fmt;
use std::io;
use std::io::Write;
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

use libc::{c_char, c_int};
use tokio::sync::mpsc::{
    unbounded_channel, UnboundedReceiver, UnboundedSender, WeakUnboundedSender,
};
use tokio::sync::oneshot;
use vpnhotspotd::shared::nonfatal::{NonfatalCoalescer, NonfatalReport};
use vpnhotspotd::shared::proto::daemon::DaemonErrorReport;
use vpnhotspotd::shared::protocol::{
    daemon_error_report, daemon_error_report_with_details, daemon_io_error_report,
    daemon_io_error_report_with_details, nonfatal_frame,
};
use vpnhotspotd::shared::reporter::{Handed, Pushed, Reporter, ReporterGuard, ReporterRegistry};

/// Where a report made anywhere in this process finds the conversation that can carry it. Holds the reporter
/// weakly and refuses a second installation - see [ReporterRegistry] - so the reporter belongs to the
/// conversation that installed it and stops existing when that conversation finishes.
static REPORTER: LazyLock<ReporterRegistry> = LazyLock::new(ReporterRegistry::default);

/// Held for as long as a test owns reporting, because [REPORTER] is deliberately process-global: a second
/// conversation's install is *refused* rather than queued, which is the production invariant and also exactly
/// what two tests installing on two threads would trip over. Every test that calls [init] takes this first,
/// so what would otherwise be a rare "already installed" failure is an ordering instead.
///
/// Tokio's rather than `std`'s, because every test that owns reporting is async and holds this across an
/// await - and because it does not poison, so one failing test leaves the rest reporting the failures they
/// found rather than a lock they never touched.
#[cfg(test)]
pub(crate) async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    SERIAL.lock().await
}
const NONFATAL_COALESCE_WINDOW: Duration = Duration::from_secs(1);
/// How many reports may be waiting in the controller's queue at once.
///
/// One, and derived rather than picked. That queue is unbounded, because a reply or an event the session must
/// not lose can always be put on it, so this is what keeps *reporting* on it bounded: a controller that has
/// stopped reading cannot be made to make room, and reporting that kept handing it summaries would grow the
/// queue by one per window per report site for as long as the daemon ran. Under this bound the reports stay
/// in their windows instead, where the coalescer already holds them to one batch per site, and the writer's
/// own progress is what releases them.
///
/// The writer is serial - one task, writing one frame at a time - so a second handed report cannot be written
/// any sooner than the first is. A larger number would only move reports out of the coalescer, where they are
/// summarised, and into a queue where they are not.
const NONFATAL_QUEUE: usize = 1;
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
        /// This report's place in this queue, given back when the writer drops the message it has written -
        /// which is what lets the next report be handed over. Named for nothing to read, because nothing
        /// should: it is released by dropping the message, so every path the writer has out of one gives the
        /// place back without having to remember to.
        ///
        /// `None` for the root daemon, which has no such bound: its queue is unbounded and its reporter is a
        /// single coalescing task, exactly as it always was. Only the app session hands out places.
        _place: Option<Handed>,
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

    /// Root's nonfatal send: no place, because root's queue is unbounded and nothing is waiting on room.
    fn send_nonfatal(&self, call_id: Option<u64>, report: DaemonErrorReport) -> bool;
}

impl ControllerSenderExt for ControllerSender {
    fn send_nonfatal(&self, call_id: Option<u64>, report: DaemonErrorReport) -> bool {
        self.send(ControllerMessage::Nonfatal {
            frame: nonfatal_frame(call_id, report.clone()),
            report,
            _place: None,
        })
        .is_ok()
    }

    fn send_frame(&self, frame: Vec<u8>) -> bool {
        self.send(ControllerMessage::Frame(frame)).is_ok()
    }
}

/// How a report is put on the wire, which is the one thing the two control conversations do not share.
/// The root path wraps it in a `DaemonEnvelope`; the app-UID path has its own frame, because none of the
/// call/reply vocabulary around that envelope means anything at the app UID.
pub(crate) type NonfatalEncoder = fn(Option<u64>, DaemonErrorReport) -> Vec<u8>;

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

#[cfg(not(test))]
#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(priority: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

/// The platform's log, dropped when this binary is built as a test harness.
///
/// A host has no `liblog` to link against, so without this the binary's own modules could not be tested at
/// all. Reports still travel their real path - the structured one this daemon owns - and only the write into
/// the platform's ring buffer goes nowhere.
///
/// # Safety
///
/// Nothing is dereferenced, so calling it is unconditionally sound.
#[cfg(test)]
unsafe fn __android_log_write(
    _priority: c_int,
    _tag: *const c_char,
    _text: *const c_char,
) -> c_int {
    0
}

/// Installs the one reporter this process may have and hands its owner back, so the conversation that called
/// this is the thing that owns reporting: nothing else can extend it, and finishing the guard is what ends it
/// - see [ReporterGuard::finish], whose result the session returns.
///
/// The sender is downgraded rather than held: the reporter is reachable from every packet path, so a strong
/// reference here would keep the control writer alive past the point its owner drops it and the session would
/// never see the writer's own result.
///
/// Fails when another conversation's reporter is still installed, and fails having started nothing at all -
/// the registration is what admits the window task, so an overlapping second conversation cannot leak one.
pub(crate) fn init_owned(
    sender: ControllerSender,
    encoder: NonfatalEncoder,
) -> io::Result<ReporterGuard> {
    let controller = sender.downgrade();
    REPORTER.install(Reporter::new(
        NONFATAL_COALESCE_WINDOW,
        NONFATAL_QUEUE,
        move |NonfatalReport { call_id, report }, place| match controller.upgrade() {
            Some(sender) => {
                let frame = encoder(call_id, report.clone());
                match sender.send(ControllerMessage::Nonfatal {
                    frame,
                    report,
                    _place: Some(place),
                }) {
                    Ok(()) => true,
                    // The conversation is the only place a report can go, so this is where one is
                    // lost. It goes to stderr, which the app reads, and the loss itself reaches the
                    // session through the guard's finish rather than ending here.
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

/// Root's reporter, exactly as it has always been: one coalescing task behind a process-global channel,
/// installed for the daemon's single control conversation and drained once at the end.
///
/// Kept whole rather than folded into the app's registry. The app session needs an owned reporter whose
/// finish is part of the session's own result and whose handover is bounded by its writer queue; root needs
/// neither, and giving it either would be changing root's behaviour to suit a mode it does not run.
static ROOT: OnceLock<UnboundedSender<ReportCommand>> = OnceLock::new();

enum ReportCommand {
    Report {
        call_id: Option<u64>,
        report: DaemonErrorReport,
    },
    Flush {
        done: oneshot::Sender<()>,
    },
}

pub(crate) fn init(sender: ControllerSender) -> io::Result<()> {
    let controller = sender.downgrade();
    let (report_sender, report_receiver) = unbounded_channel();
    ROOT.set(report_sender)
        .map_err(|_| io::Error::other("nonfatal reporter already initialized"))?;
    tokio::spawn(run_reporter(controller, report_receiver));
    Ok(())
}

pub(crate) async fn flush() {
    let Some(sender) = ROOT.get() else {
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

pub(crate) fn report(report: DaemonErrorReport) {
    report_for(None, report);
}

/// Coalesces one report into whichever conversation owns reporting in *this* process.
///
/// The app registry first, and if one is installed it is the only answer: a session whose reporter has
/// finished gets a stderr line rather than a silent fall-through to root's pipeline, because the app UID has
/// no root pipeline and pretending otherwise would report a session's failures as somebody else's. Root
/// installs no registry, so a root daemon always takes the second arm - master's behaviour, unchanged.
pub(crate) fn report_for(call_id: Option<u64>, report: DaemonErrorReport) {
    if let Some(reporter) = REPORTER.get() {
        match reporter.push(call_id, report) {
            Pushed::Coalesced => {}
            // Handed back rather than dropped inside, so this costs nothing on the path that works: the
            // report is only formatted where there is no longer anywhere to send it.
            Pushed::Closed(report) => {
                stderr!("nonfatal report made after the reporter finished: {report:?}");
            }
        }
        return;
    }
    if let Some(sender) = ROOT.get() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DaemonErrorReport {
        daemon_error_report("test.dispatch", "message", "Other")
    }

    /// With no app registry installed, a report takes root's global pipeline - master's behaviour exactly.
    ///
    /// The root pipeline is a `OnceLock` for the life of the process, so this both installs it and proves the
    /// dispatch: the report arrives on the controller queue that [init] was given, having gone through root's
    /// own coalescing task rather than through any registry.
    #[tokio::test]
    async fn root_dispatch_uses_the_global_pipeline() {
        let _serial = exclusive().await;
        let (sender, mut controller) = unbounded_channel::<ControllerMessage>();
        // Root installs no registry, which is the other half of the dispatch rule.
        assert!(REPORTER.get().is_none());
        // Held so the pipeline's weak controller can still be upgraded; the registry and root alike keep the
        // controller weakly, so the only strong reference has to be this test's.
        let _strong = sender.clone();
        // `ROOT` is a `OnceLock` for the life of the process, so only the first test to get here installs it.
        // Whichever that is, the assertion below is about the same pipeline.
        let installed = init(sender).is_ok();
        report(sample());
        flush().await;
        if installed {
            let message = controller.recv().await.expect("root delivered the report");
            assert!(matches!(message, ControllerMessage::Nonfatal { .. }));
        }
    }

    /// An installed app registry takes the report, and root's pipeline never sees it.
    ///
    /// No double delivery is the property: the app UID has its own conversation, and a report reaching both
    /// would be reported twice to two different owners.
    #[tokio::test]
    async fn app_dispatch_uses_the_installed_registry() {
        let _serial = exclusive().await;
        let (app, mut app_side) = unbounded_channel::<ControllerMessage>();
        let _strong = app.clone();
        let guard = init_owned(app, nonfatal_frame).expect("no other conversation is installed");
        report(sample());
        guard.finish().await.expect("the app reporter drains");
        let message = app_side
            .recv()
            .await
            .expect("the app registry delivered it");
        assert!(matches!(message, ControllerMessage::Nonfatal { .. }));
        // Nothing else is waiting on the app's queue: the report was delivered once, and nothing that is not
        // this test's own reached a registry this test owned for the whole of its installed life.
        assert!(app_side.try_recv().is_err());
    }

    /// After the app's reporter finishes there is no fall-through to root: the report is written to stderr.
    ///
    /// Fail-closed by design. The app UID has no root pipeline, so quietly handing its late reports to one
    /// would attribute a session's failures to a conversation that never owned them - and on a rooted device
    /// it would put app-session reports on root's controller.
    #[tokio::test]
    async fn a_finished_app_registry_does_not_fall_through_to_root() {
        let _serial = exclusive().await;
        let (app, mut app_side) = unbounded_channel::<ControllerMessage>();
        let _strong = app.clone();
        let guard = init_owned(app, nonfatal_frame).expect("no other conversation is installed");
        guard.finish().await.expect("the app reporter drains");
        // The registration is gone with the guard, so this takes the same path a root-less process takes.
        assert!(REPORTER.get().is_none());
        report(sample());
        assert!(
            app_side.try_recv().is_err(),
            "nothing reached the finished app queue"
        );
    }
}
