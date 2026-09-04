//! Applies app-UID admission configuration.
use std::io;

use prost::Message;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::{mpsc, oneshot};
use vpnhotspotd::shared::app_control::{self, Rejected, Request};
use vpnhotspotd::shared::proto::daemon::{self, DaemonErrorReport};
use vpnhotspotd::shared::protocol::{ack_reply_frame, describe_io_error, IoErrorReportExt};

use crate::control_wire::recv_packet;
use crate::report::{self, ControllerSender, ControllerSenderExt};
use crate::shizuku::tun_reader::Applied;

/// How the config loop ended, when it did not end with a failure of the session's own.
pub(super) enum Ended {
    /// Nothing more is owed on the start call. Either the app closed the control socket, so there is nobody
    /// left to answer, or it cancelled the start call - which removes the call on the app's side before the
    /// cancel is even written, exactly as the root controller does.
    Silent,
    /// A config was refused, and its own `ErrorFrame` is the answer. The session ends with that failure, but
    /// the start call gets no second frame carrying the same thing: one failure must not reach the app twice.
    Refused {
        call_id: u64,
        /// Built where the refusal happened, so its context, errno, details and Rust source location are that
        /// site's rather than the teardown's.
        report: DaemonErrorReport,
        /// The same failure as the session's own result, which is what the process ends with.
        error: io::Error,
    },
}

/// Reads config calls until something ends the session.
pub(super) async fn serve(
    stream: &mut OwnedReadHalf,
    session: u64,
    interface_name: &str,
    configs: &mpsc::Sender<Applied>,
    control: &ControllerSender,
) -> io::Result<Ended> {
    loop {
        let packet = match recv_packet(stream).await {
            Ok(packet) => packet,
            // EOF on the control socket is the authoritative cancellation signal
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Ended::Silent),
            Err(e) => return Err(e.with_report_context("shizuku.control.recv_packet")),
        };
        let envelope = daemon::ClientEnvelope::decode(packet.as_slice()).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, e)
                .with_report_context("shizuku.control.parse_frame")
        })?;
        let (call_id, config) = match app_control::read_request(envelope, session) {
            Ok(Request::Config { call_id, config }) => (call_id, config),
            Ok(Request::CancelSession) => return Ok(Ended::Silent),
            Ok(Request::CancelStale) => continue,
            Err(Rejected {
                call_id: Some(call_id),
                error,
            }) => return Ok(refuse(call_id, error)),
            Err(Rejected {
                call_id: None,
                error,
            }) => return Err(error),
        };
        report::stdout!(
            "applying config call {call_id} on {interface_name}: admitting {}",
            config.admit,
        );
        let (applied, acknowledged) = oneshot::channel();
        let published = Applied {
            admitting: config.admit,
            applied,
        };
        if configs.send(published).await.is_err() {
            return Ok(refuse(
                call_id,
                io::Error::other("tun ingress stopped before the session")
                    .with_report_context("shizuku.control.config"),
            ));
        }
        // The reply waits until the task that owns admission has applied this value.
        if acknowledged.await.is_err() {
            return Err(io::Error::other("tun ingress dropped a config it accepted")
                .with_report_context("shizuku.control.config"));
        }
        if !control.send_frame(ack_reply_frame(call_id)) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the control writer stopped before the session",
            )
            .with_report_context("shizuku.control.config"));
        }
    }
}

/// Owes one config call the failure that refused it, so the app learns it as itself.
#[track_caller]
fn refuse(call_id: u64, error: io::Error) -> Ended {
    Ended::Refused {
        call_id,
        report: describe_io_error("shizuku.control.config", &error, [("call_id", call_id)]),
        error,
    }
}
