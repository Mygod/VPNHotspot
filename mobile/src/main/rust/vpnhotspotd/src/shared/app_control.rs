use std::io;
use std::net::IpAddr;

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
#[derive(Debug, PartialEq, Eq)]
pub struct Start {
    pub call_id: u64,
    pub interface_name: String,
    pub mtu: u32,
    pub virtual_addresses: Vec<IpAddr>,
    pub gateway_addresses: Vec<IpAddr>,
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
    /// A cancel for an already-answered config call; the serial conversation has nothing left to cancel.
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
            virtual_addresses: addresses("virtual", &command.virtual_addresses)
                .map_err(|message| refused(call_id, message))?,
            gateway_addresses: addresses("gateway", &command.gateway_addresses)
                .map_err(|message| refused(call_id, message))?,
        }),
        other => Err(refused(
            call_id,
            format!("{} is not a session start", describe(&other)),
        )),
    }
}

/// One frame on the established session, checked against the start call that owns it.
pub fn read_request(envelope: daemon::ClientEnvelope, session: u64) -> Result<Request, Rejected> {
    let call_id = read_call_id(&envelope)?;
    match envelope.command {
        Some(Command::Cancel(_)) => Ok(if call_id == session {
            Request::CancelSession
        } else {
            Request::CancelStale
        }),
        Some(Command::ApplyShizukuConfig(config)) => {
            // The start call is answered with an ACK, terminal errors and a completion for the session's
            // whole life, so a config sharing its ID would make a reply ambiguous between the two.
            if call_id == session {
                return Err(refused(
                    call_id,
                    "a config cannot reuse the session's own call id".to_owned(),
                ));
            }
            Ok(Request::Config { call_id, config })
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

fn addresses(field: &str, packed: &[Vec<u8>]) -> Result<Vec<IpAddr>, String> {
    packed
        .iter()
        .map(|address| match address.len() {
            4 => Ok(IpAddr::from(<[u8; 4]>::try_from(&address[..]).unwrap())),
            16 => Ok(IpAddr::from(<[u8; 16]>::try_from(&address[..]).unwrap())),
            size => Err(format!("{field} address has {size} bytes")),
        })
        .collect()
}

/// Names the command family without dumping its payload into an error.
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
                virtual_addresses: vec![vec![192, 0, 2, 5]],
                gateway_addresses: vec![vec![192, 0, 2, 1]],
            },
        ))
    }

    fn config() -> Option<Command> {
        Some(Command::ApplyShizukuConfig(
            daemon::ShizukuSessionConfig::default(),
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
                virtual_addresses: vec!["192.0.2.5".parse().unwrap()],
                gateway_addresses: vec!["192.0.2.1".parse().unwrap()],
            }
        );
    }

    #[test]
    fn a_frame_without_a_call_id_can_be_answered_nowhere() {
        let rejected = read_start_call(envelope(0, start("testtun0", 1500))).unwrap_err();

        assert_eq!(rejected.call_id, None);
        assert_eq!(rejected.error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            read_request(envelope(0, config()), SESSION)
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
            read_request(envelope(9, config()), SESSION).unwrap(),
            Request::Config {
                call_id: 9,
                config: daemon::ShizukuSessionConfig::default(),
            }
        );
    }

    #[test]
    fn a_config_may_not_reuse_the_session_call() {
        let rejected = read_request(envelope(SESSION, config()), SESSION).unwrap_err();

        assert_eq!(rejected.call_id, Some(SESSION));
        assert!(
            rejected.error.to_string().contains("own call id"),
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
