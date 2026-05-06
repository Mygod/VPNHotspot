use std::io;
use std::sync::OnceLock;

use tokio::sync::mpsc::{UnboundedSender, WeakUnboundedSender};
use vpnhotspotd::shared::protocol::DaemonErrorReport;
use vpnhotspotd::shared::transport::nonfatal_frame;

static REPORTER: OnceLock<WeakUnboundedSender<Vec<u8>>> = OnceLock::new();

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
    eprintln!("nonfatal report dropped after controller disconnect: {report:?}");
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
