use std::ffi::CString;
use std::fmt;
use std::io;
use std::io::Write;
use std::sync::OnceLock;

use libc::{c_char, c_int};
use tokio::sync::mpsc::{UnboundedSender, WeakUnboundedSender};
use vpnhotspotd::shared::protocol::DaemonErrorReport;
use vpnhotspotd::shared::transport::nonfatal_frame;

static REPORTER: OnceLock<WeakUnboundedSender<Vec<u8>>> = OnceLock::new();
const ANDROID_LOG_INFO: c_int = 4;
const ANDROID_LOG_ERROR: c_int = 6;
const LOG_TAG: &[u8] = b"vpnhotspotd\0";

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

pub(crate) fn init(sender: UnboundedSender<Vec<u8>>) -> io::Result<()> {
    REPORTER
        .set(sender.downgrade())
        .map_err(|_| io::Error::other("nonfatal reporter already initialized"))
}

pub(crate) fn report(report: DaemonErrorReport) {
    report_for(None, report);
}

pub(crate) fn report_for(call_id: Option<u64>, report: DaemonErrorReport) {
    if let Some(sender) = REPORTER.get().and_then(WeakUnboundedSender::upgrade) {
        if sender.send(nonfatal_frame(call_id, report.clone())).is_ok() {
            return;
        }
    }
    stderr!("nonfatal report dropped after controller disconnect: {report:?}");
}

#[track_caller]
pub(crate) fn io(context: impl Into<String>, error: io::Error) {
    report(DaemonErrorReport::from_io_error(context, error));
}

#[track_caller]
pub(crate) fn message(
    context: impl Into<String>,
    message: impl Into<String>,
    kind: impl Into<String>,
) {
    report(DaemonErrorReport::from_message(context, message, kind));
}

#[track_caller]
pub(crate) fn io_with_details<I, K, V>(context: impl Into<String>, error: io::Error, details: I)
where
    I: IntoIterator<Item = (K, V)>,
    K: ToString,
    V: ToString,
{
    report(DaemonErrorReport::from_io_error_with_details(
        context, error, details,
    ));
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
