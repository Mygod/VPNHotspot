//! What the app-UID control conversation accepts, and what refusing a frame looks like.
//!
//! Pure decoding, and deliberately apart from the loop that reads the socket: every rule below is a
//! comparison over one already-decoded envelope, which is what makes the whole vocabulary - including the
//! two refusals that keep the root and app-UID command families apart - testable without a device, a TUN or
//! a controller.
//!
//! The distinction this module exists to make is between a frame that names a call and one that does not. A
//! refusal naming a call is answered on it as an `ErrorFrame`, which is how a Rust failure reaches the app
//! as itself rather than as a closed socket; a refusal that cannot name one has nowhere to go, and ends the
//! conversation as the transport failure it is.

use std::io;

use crate::shared::proto::daemon::{self, client_envelope::Command};
use crate::shared::protocol::IoErrorReportExt;

/// Why a frame was refused, and where the refusal can be delivered.
#[derive(Debug)]
pub struct Rejected {
    /// The call to answer on, or `None` when the frame carried no usable call ID at all. Nothing can be
    /// answered in that case, so the caller ends the conversation instead of reporting into it.
    pub call_id: Option<u64>,
    pub error: io::Error,
}

/// The session start carried by the first frame of an app-UID connection.
///
/// Its call ID owns the dataplane for as long as the session runs: the readiness ACK, every terminal error
/// and the completion all name it, and the configs below are keyed to it.
#[derive(Debug, PartialEq, Eq)]
pub struct Start {
    pub call_id: u64,
    pub interface_name: String,
    pub mtu: u32,
}

/// What one frame on an established app-UID session asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    /// A level-triggered config, to be answered on this call ID and no other.
    Config {
        call_id: u64,
        config: daemon::ShizukuSessionConfig,
    },
    /// A cancel naming the start call, which is the app asking for the session itself to end.
    CancelSession,
    /// A cancel naming a call that is no longer active. That is what an app whose caller was cancelled while
    /// a config was in flight leaves behind, and it asks for nothing: this conversation is serial, so the
    /// cancel is read strictly after the config it names was applied and replied to, and there is no longer
    /// anything to abandon.
    CancelStale,
}

/// The first frame on an app-UID connection, which may only be a session start.
pub fn read_start_call(envelope: daemon::ClientEnvelope) -> Result<Start, Rejected> {
    let call_id = read_call_id(&envelope)?;
    match envelope.command {
        Some(Command::StartShizukuSession(command)) => Ok(Start {
            call_id,
            interface_name: command.interface_name,
            mtu: command.mtu,
        }),
        other => Err(refused(
            call_id,
            format!("{} is not a session start", describe(&other)),
        )),
    }
}

/// One frame on the established session, checked against the start call that owns it.
///
/// A second start is refused here rather than served, because the descriptor and the dataplane belong to the
/// call that already has them: one connection is one session.
pub fn read_request(envelope: daemon::ClientEnvelope, session: u64) -> Result<Request, Rejected> {
    let call_id = read_call_id(&envelope)?;
    match envelope.command {
        Some(Command::Cancel(_)) => Ok(if call_id == session {
            Request::CancelSession
        } else {
            Request::CancelStale
        }),
        Some(Command::ApplyShizukuConfig(command)) => {
            // The start call is answered with an ACK, terminal errors and a completion for the session's
            // whole life, so a config sharing its ID would make a reply ambiguous between the two.
            if call_id == session {
                return Err(refused(
                    call_id,
                    "a config cannot reuse the session's own call id".to_owned(),
                ));
            }
            if command.session_id != session {
                return Err(refused(
                    call_id,
                    format!(
                        "config names session {} but this connection is session {session}",
                        command.session_id
                    ),
                ));
            }
            match command.config {
                Some(config) => Ok(Request::Config { call_id, config }),
                None => Err(refused(call_id, "config call carries no config".to_owned())),
            }
        }
        other => Err(refused(
            call_id,
            format!("{} is not a config", describe(&other)),
        )),
    }
}

/// Zero is what an unset proto field decodes to, so it cannot name a call and nothing can be answered on it.
fn read_call_id(envelope: &daemon::ClientEnvelope) -> Result<u64, Rejected> {
    if envelope.call_id == 0 {
        return Err(Rejected {
            call_id: None,
            error: io::Error::new(io::ErrorKind::InvalidData, "invalid daemon call id 0")
                .with_report_context("shizuku.control.call_id"),
        });
    }
    Ok(envelope.call_id)
}

fn refused(call_id: u64, message: String) -> Rejected {
    Rejected {
        call_id: Some(call_id),
        error: io::Error::new(io::ErrorKind::InvalidInput, message)
            .with_report_context_details("shizuku.control.command", [("call_id", call_id)]),
    }
}

/// Names the command a refusal is about without printing it. A root command carries a whole `SessionConfig`
/// and a config call a whole address set, neither of which belongs in an error message - what the reader
/// needs is which family the sender used, since that is the boundary being enforced.
fn describe(command: &Option<Command>) -> &'static str {
    match command {
        None => "no command",
        Some(Command::Cancel(_)) => "a cancel",
        Some(Command::StartSession(_)) => "a root session start",
        Some(Command::ReplaceSession(_)) => "a root session replacement",
        Some(Command::ReadTrafficCounters(_)) => "a root traffic counter read",
        Some(Command::StartNeighbourMonitor(_)) => "a root neighbour monitor start",
        Some(Command::CleanRouting(_)) => "a root routing clean",
        Some(Command::ReplaceStaticAddresses(_)) => "a root static address replacement",
        Some(Command::StartShizukuSession(_)) => "a session start",
        Some(Command::ApplyShizukuConfig(_)) => "a config",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: u64 = 7;

    fn envelope(call_id: u64, command: Option<Command>) -> daemon::ClientEnvelope {
        daemon::ClientEnvelope { call_id, command }
    }

    fn start(interface_name: &str, mtu: u32) -> Option<Command> {
        Some(Command::StartShizukuSession(
            daemon::StartShizukuSessionCommand {
                interface_name: interface_name.to_owned(),
                mtu,
            },
        ))
    }

    fn config(session_id: u64) -> Option<Command> {
        Some(Command::ApplyShizukuConfig(
            daemon::ApplyShizukuConfigCommand {
                session_id,
                config: Some(daemon::ShizukuSessionConfig::default()),
            },
        ))
    }

    #[test]
    fn start_call_is_read_with_its_declared_interface() {
        assert_eq!(
            read_start_call(envelope(3, start("testtun0", 1500))).unwrap(),
            Start {
                call_id: 3,
                interface_name: "testtun0".to_owned(),
                mtu: 1500,
            }
        );
    }

    #[test]
    fn a_frame_without_a_call_id_can_be_answered_nowhere() {
        let rejected = read_start_call(envelope(0, start("testtun0", 1500))).unwrap_err();

        assert_eq!(rejected.call_id, None);
        assert_eq!(rejected.error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            read_request(envelope(0, config(SESSION)), SESSION)
                .unwrap_err()
                .call_id,
            None
        );
    }

    #[test]
    fn a_root_command_is_refused_on_its_own_call() {
        let root = Some(Command::ReadTrafficCounters(
            daemon::ReadTrafficCountersCommand {},
        ));
        let rejected = read_start_call(envelope(3, root.clone())).unwrap_err();

        assert_eq!(rejected.call_id, Some(3));
        assert!(
            rejected
                .error
                .to_string()
                .contains("a root traffic counter read"),
            "{}",
            rejected.error
        );
        assert_eq!(
            read_request(envelope(9, root), SESSION)
                .unwrap_err()
                .call_id,
            Some(9)
        );
    }

    #[test]
    fn an_empty_envelope_is_refused_on_its_own_call() {
        let rejected = read_start_call(envelope(3, None)).unwrap_err();

        assert_eq!(rejected.call_id, Some(3));
        assert!(
            rejected.error.to_string().contains("no command"),
            "{}",
            rejected.error
        );
    }

    #[test]
    fn a_config_is_read_under_the_call_that_must_answer_it() {
        assert_eq!(
            read_request(envelope(9, config(SESSION)), SESSION).unwrap(),
            Request::Config {
                call_id: 9,
                config: daemon::ShizukuSessionConfig::default(),
            }
        );
    }

    #[test]
    fn a_config_for_another_session_is_refused() {
        let rejected = read_request(envelope(9, config(SESSION + 1)), SESSION).unwrap_err();

        assert_eq!(rejected.call_id, Some(9));
        assert!(
            rejected.error.to_string().contains("names session 8"),
            "{}",
            rejected.error
        );
    }

    #[test]
    fn a_config_may_not_reuse_the_session_call() {
        let rejected = read_request(envelope(SESSION, config(SESSION)), SESSION).unwrap_err();

        assert_eq!(rejected.call_id, Some(SESSION));
        assert!(
            rejected.error.to_string().contains("own call id"),
            "{}",
            rejected.error
        );
    }

    #[test]
    fn a_config_call_without_a_config_is_refused() {
        let empty = Some(Command::ApplyShizukuConfig(
            daemon::ApplyShizukuConfigCommand {
                session_id: SESSION,
                config: None,
            },
        ));
        let rejected = read_request(envelope(9, empty), SESSION).unwrap_err();

        assert_eq!(rejected.call_id, Some(9));
        assert!(
            rejected.error.to_string().contains("carries no config"),
            "{}",
            rejected.error
        );
    }

    #[test]
    fn a_cancel_is_the_session_only_when_it_names_the_start_call() {
        let cancel = || Some(Command::Cancel(daemon::CancelCommand {}));

        assert_eq!(
            read_request(envelope(SESSION, cancel()), SESSION).unwrap(),
            Request::CancelSession
        );
        assert_eq!(
            read_request(envelope(9, cancel()), SESSION).unwrap(),
            Request::CancelStale
        );
    }

    #[test]
    fn a_second_start_is_refused_rather_than_served() {
        let rejected = read_request(envelope(9, start("testtun1", 1500)), SESSION).unwrap_err();

        assert_eq!(rejected.call_id, Some(9));
        assert!(
            rejected.error.to_string().contains("is not a config"),
            "{}",
            rejected.error
        );
    }
}
