use std::error::Error;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::panic::Location;
use std::process;

use cidr::{Ipv6Cidr, Ipv6Inet};
use prost::Message;

use crate::shared::model::{
    ipv6_nat_gateway, ipv6_nat_prefix, ClientConfig, Ipv6NatConfig, MasqueradeMode, SessionConfig,
    DAEMON_REPLY_MARK,
};
use crate::shared::proto::daemon;

const MAX_ERROR_DETAILS: usize = 32;
const MAX_ERROR_FIELD_BYTES: usize = 4096;

#[derive(Debug)]
pub enum Command {
    Cancel,
    StartSession(SessionConfig),
    ReplaceSession {
        session_id: u64,
        config: SessionConfig,
    },
    ReadTrafficCounters,
    StartNeighbourMonitor,
    CleanRouting(CleanRoutingCommand),
    ReplaceStaticAddresses(StaticAddressesCommand),
    DeleteStaticAddresses {
        interface: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NeighbourState {
    Unset = 0,
    Incomplete = 1,
    Valid = 2,
    Failed = 3,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Neighbour {
    pub address: IpAddr,
    pub interface: String,
    pub lladdr: Option<[u8; 6]>,
    pub state: NeighbourState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NeighbourDelta {
    Upsert(Neighbour),
    Delete { address: IpAddr, interface: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpAddressEntry {
    pub address: IpAddr,
    pub prefix_len: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StaticAddressesCommand {
    pub interface: String,
    pub addresses: Vec<IpAddressEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CleanRoutingCommand {
    pub ipv6_nat_prefix_seed: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonErrorReport {
    pub context: String,
    pub message: String,
    pub errno: Option<i32>,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub pid: u32,
    pub details: Vec<(String, String)>,
}

impl DaemonErrorReport {
    #[track_caller]
    pub fn from_message(
        context: impl Into<String>,
        message: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        Self::from_message_with_details(context, message, kind, std::iter::empty::<(&str, &str)>())
    }

    #[track_caller]
    pub fn from_message_with_details<I, K, V>(
        context: impl Into<String>,
        message: impl Into<String>,
        kind: impl Into<String>,
        details: I,
    ) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: ToString,
        V: ToString,
    {
        let location = Location::caller();
        Self {
            context: trim_error_field(context.into()),
            message: trim_error_field(message.into()),
            errno: None,
            kind: trim_error_field(kind.into()),
            file: trim_error_field(location.file().to_owned()),
            line: location.line(),
            column: location.column(),
            pid: process::id(),
            details: details
                .into_iter()
                .take(MAX_ERROR_DETAILS)
                .map(|(key, value)| {
                    (
                        trim_error_field(key.to_string()),
                        trim_error_field(value.to_string()),
                    )
                })
                .collect(),
        }
    }

    #[track_caller]
    pub fn from_io_error(context: impl Into<String>, error: io::Error) -> Self {
        Self::from_io_error_with_details(context, error, std::iter::empty::<(&str, &str)>())
    }

    #[track_caller]
    pub fn from_io_error_with_details<I, K, V>(
        context: impl Into<String>,
        error: io::Error,
        details: I,
    ) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: ToString,
        V: ToString,
    {
        if let Some(report) = Self::from_reported_io_error(&error) {
            return report;
        }
        let location = Location::caller();
        Self {
            context: trim_error_field(context.into()),
            message: trim_error_field(error.to_string()),
            errno: error.raw_os_error(),
            kind: trim_error_field(format!("{:?}", error.kind())),
            file: trim_error_field(location.file().to_owned()),
            line: location.line(),
            column: location.column(),
            pid: process::id(),
            details: details
                .into_iter()
                .take(MAX_ERROR_DETAILS)
                .map(|(key, value)| {
                    (
                        trim_error_field(key.to_string()),
                        trim_error_field(value.to_string()),
                    )
                })
                .collect(),
        }
    }

    fn from_reported_io_error(error: &io::Error) -> Option<Self> {
        error
            .get_ref()?
            .downcast_ref::<DaemonReportError>()
            .map(|error| error.report.clone())
    }
}

#[derive(Debug)]
pub struct DaemonReportError {
    report: DaemonErrorReport,
}

impl DaemonReportError {
    pub fn report(&self) -> &DaemonErrorReport {
        &self.report
    }
}

impl fmt::Display for DaemonReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {}",
            self.report.context, self.report.message
        )
    }
}

impl Error for DaemonReportError {}

pub fn error_errno(error: &io::Error) -> Option<i32> {
    DaemonErrorReport::from_reported_io_error(error)
        .and_then(|report| report.errno)
        .or_else(|| error.raw_os_error())
}

pub trait IoErrorReportExt {
    #[track_caller]
    fn with_report_context(self, context: impl Into<String>) -> Self;

    #[track_caller]
    fn with_report_context_details<I, K, V>(self, context: impl Into<String>, details: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: ToString,
        V: ToString;
}

impl IoErrorReportExt for io::Error {
    #[track_caller]
    fn with_report_context(self, context: impl Into<String>) -> Self {
        self.with_report_context_details(context, std::iter::empty::<(&str, &str)>())
    }

    #[track_caller]
    fn with_report_context_details<I, K, V>(self, context: impl Into<String>, details: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: ToString,
        V: ToString,
    {
        if DaemonErrorReport::from_reported_io_error(&self).is_some() {
            return self;
        }
        let kind = self.kind();
        let report = DaemonErrorReport::from_io_error_with_details(context, self, details);
        io::Error::new(kind, DaemonReportError { report })
    }
}

pub trait IoResultReportExt<T> {
    #[track_caller]
    fn with_report_context(self, context: impl Into<String>) -> Self;

    #[track_caller]
    fn with_report_context_details<I, K, V>(self, context: impl Into<String>, details: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: ToString,
        V: ToString;
}

impl<T> IoResultReportExt<T> for io::Result<T> {
    #[track_caller]
    fn with_report_context(self, context: impl Into<String>) -> Self {
        self.map_err(|error| error.with_report_context(context))
    }

    #[track_caller]
    fn with_report_context_details<I, K, V>(self, context: impl Into<String>, details: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: ToString,
        V: ToString,
    {
        self.map_err(|error| error.with_report_context_details(context, details))
    }
}

pub fn parse_client_packet(packet: &[u8]) -> io::Result<(u64, io::Result<Command>)> {
    let envelope = daemon::ClientEnvelope::decode(packet).map_err(invalid_decode_error)?;
    let id = read_call_id(envelope.call_id)?;
    Ok((id, parse_command(envelope.command)))
}

pub fn ack_reply_frame(id: u64) -> Vec<u8> {
    reply_frame(id, daemon::reply_frame::Payload::Ack(daemon::Ack {}))
}

pub fn traffic_counter_lines_frame(id: u64, lines: &[String]) -> Vec<u8> {
    reply_frame(
        id,
        daemon::reply_frame::Payload::TrafficCounterLines(daemon::TrafficCounterLines {
            lines: lines.to_vec(),
        }),
    )
}

pub fn ack_event_frame(id: u64) -> Vec<u8> {
    event_frame(id, daemon::event_frame::Payload::Ack(daemon::Ack {}))
}

pub fn neighbour_deltas_frame<I>(id: u64, deltas: I) -> Vec<u8>
where
    I: IntoIterator<Item = NeighbourDelta>,
{
    event_frame(
        id,
        daemon::event_frame::Payload::NeighbourDeltas(daemon::NeighbourDeltas {
            deltas: deltas.into_iter().map(neighbour_delta_proto).collect(),
        }),
    )
}

pub fn complete_frame(id: u64) -> Vec<u8> {
    daemon_envelope(daemon::daemon_envelope::Frame::Complete(
        daemon::CompleteFrame {
            call_id: checked_call_id(id),
        },
    ))
}

pub fn error_frame(id: u64, report: DaemonErrorReport) -> Vec<u8> {
    daemon_envelope(daemon::daemon_envelope::Frame::Error(daemon::ErrorFrame {
        call_id: checked_call_id(id),
        report: Some(error_report_proto(&report)),
    }))
}

pub fn nonfatal_frame(id: Option<u64>, report: DaemonErrorReport) -> Vec<u8> {
    daemon_envelope(daemon::daemon_envelope::Frame::NonFatal(
        daemon::NonFatalFrame {
            call_id: id.map(checked_call_id),
            report: Some(error_report_proto(&report)),
        },
    ))
}

fn parse_command(command: Option<daemon::client_envelope::Command>) -> io::Result<Command> {
    match command.ok_or_else(|| invalid_data("missing command"))? {
        daemon::client_envelope::Command::Cancel(_) => Ok(Command::Cancel),
        daemon::client_envelope::Command::StartSession(command) => Ok(Command::StartSession(
            read_session_config(required(command.config, "start session config")?)?,
        )),
        daemon::client_envelope::Command::ReplaceSession(command) => Ok(Command::ReplaceSession {
            session_id: command.session_id,
            config: read_session_config(required(command.config, "replace session config")?)?,
        }),
        daemon::client_envelope::Command::ReadTrafficCounters(_) => {
            Ok(Command::ReadTrafficCounters)
        }
        daemon::client_envelope::Command::StartNeighbourMonitor(_) => {
            Ok(Command::StartNeighbourMonitor)
        }
        daemon::client_envelope::Command::CleanRouting(command) => {
            Ok(Command::CleanRouting(CleanRoutingCommand {
                ipv6_nat_prefix_seed: command.ipv6_nat_prefix_seed,
            }))
        }
        daemon::client_envelope::Command::ReplaceStaticAddresses(command) => Ok(
            Command::ReplaceStaticAddresses(read_static_addresses_command(command)?),
        ),
        daemon::client_envelope::Command::DeleteStaticAddresses(command) => {
            Ok(Command::DeleteStaticAddresses {
                interface: command.interface,
            })
        }
    }
}

fn read_session_config(config: daemon::SessionConfig) -> io::Result<SessionConfig> {
    let downstream = config.downstream;
    let primary_routes = config
        .primary_routes
        .iter()
        .map(read_ipv6_prefix)
        .collect::<io::Result<_>>()?;
    let clients = config
        .clients
        .into_iter()
        .map(read_client)
        .collect::<io::Result<_>>()?;
    let ipv6_nat = if let Some(ipv6_nat) = config.ipv6_nat {
        let prefix = ipv6_nat_prefix(&ipv6_nat.prefix_seed, &downstream);
        let gateway = ipv6_nat_gateway(prefix);
        Some(Ipv6NatConfig { gateway })
    } else {
        None
    };
    Ok(SessionConfig {
        downstream,
        reply_mark: DAEMON_REPLY_MARK,
        ip_forward: config.ip_forward,
        masquerade: read_masquerade_mode(config.masquerade)?,
        ipv6_block: config.ipv6_block,
        primary_network: config.primary_network.filter(|network| *network != 0),
        primary_routes,
        fallback_network: config.fallback_network.filter(|network| *network != 0),
        primary_upstream_interfaces: config.primary_upstream_interfaces,
        fallback_upstream_interfaces: config.fallback_upstream_interfaces,
        clients,
        ipv6_nat,
    })
}

fn read_masquerade_mode(value: i32) -> io::Result<MasqueradeMode> {
    match value {
        0 => Ok(MasqueradeMode::None),
        1 => Ok(MasqueradeMode::Simple),
        2 => Ok(MasqueradeMode::Netd),
        value => Err(invalid_data(format!("invalid masquerade mode {value}"))),
    }
}

fn read_client(client: daemon::ClientConfig) -> io::Result<ClientConfig> {
    let mac = read_mac(&client.mac, "client mac")?;
    let ipv4 = client
        .ipv4
        .iter()
        .map(|address| read_ipv4(address, "client IPv4 address"))
        .collect::<io::Result<_>>()?;
    Ok(ClientConfig { mac, ipv4 })
}

fn read_ipv6_prefix(prefix: &daemon::Ipv6Prefix) -> io::Result<Ipv6Cidr> {
    let address = read_ipv6(&prefix.address, "IPv6 route address")?;
    let prefix_len = read_prefix_len(prefix.prefix_length, 128, "IPv6 route prefix length")?;
    Ok(Ipv6Inet::new(address, prefix_len)
        .expect("route prefix length was already validated")
        .network())
}

fn read_static_addresses_command(
    command: daemon::ReplaceStaticAddressesCommand,
) -> io::Result<StaticAddressesCommand> {
    let addresses = command
        .addresses
        .iter()
        .map(|address| {
            let ip = read_ip_address(&address.address, "static address")?;
            let prefix_len = read_prefix_len(
                address.prefix_length,
                match ip {
                    IpAddr::V4(_) => 32,
                    IpAddr::V6(_) => 128,
                },
                "static address prefix length",
            )?;
            Ok(IpAddressEntry {
                address: ip,
                prefix_len,
            })
        })
        .collect::<io::Result<_>>()?;
    Ok(StaticAddressesCommand {
        interface: command.interface,
        addresses,
    })
}

fn reply_frame(id: u64, payload: daemon::reply_frame::Payload) -> Vec<u8> {
    daemon_envelope(daemon::daemon_envelope::Frame::Reply(daemon::ReplyFrame {
        call_id: checked_call_id(id),
        payload: Some(payload),
    }))
}

fn event_frame(id: u64, payload: daemon::event_frame::Payload) -> Vec<u8> {
    daemon_envelope(daemon::daemon_envelope::Frame::Event(daemon::EventFrame {
        call_id: checked_call_id(id),
        payload: Some(payload),
    }))
}

fn daemon_envelope(frame: daemon::daemon_envelope::Frame) -> Vec<u8> {
    daemon::DaemonEnvelope { frame: Some(frame) }.encode_to_vec()
}

fn error_report_proto(report: &DaemonErrorReport) -> daemon::DaemonErrorReport {
    daemon::DaemonErrorReport {
        context: report.context.clone(),
        message: report.message.clone(),
        errno: report.errno,
        kind: report.kind.clone(),
        file: report.file.clone(),
        line: report.line,
        column: report.column,
        pid: report.pid,
        details: report
            .details
            .iter()
            .map(|(key, value)| daemon::ErrorDetail {
                key: key.clone(),
                value: value.clone(),
            })
            .collect(),
    }
}

fn neighbour_delta_proto(delta: NeighbourDelta) -> daemon::NeighbourDelta {
    daemon::NeighbourDelta {
        delta: Some(match delta {
            NeighbourDelta::Upsert(neighbour) => {
                daemon::neighbour_delta::Delta::Upsert(daemon::Neighbour {
                    address: ip_address_bytes(neighbour.address),
                    interface: neighbour.interface,
                    lladdr: neighbour.lladdr.map(Vec::from),
                    state: neighbour.state as i32,
                })
            }
            NeighbourDelta::Delete { address, interface } => {
                daemon::neighbour_delta::Delta::Delete(daemon::NeighbourDelete {
                    address: ip_address_bytes(address),
                    interface,
                })
            }
        }),
    }
}

fn ip_address_bytes(address: IpAddr) -> Vec<u8> {
    match address {
        IpAddr::V4(address) => address.octets().to_vec(),
        IpAddr::V6(address) => address.octets().to_vec(),
    }
}

fn read_call_id(id: u64) -> io::Result<u64> {
    if id == 0 {
        Err(invalid_data("invalid daemon call id 0"))
    } else {
        Ok(id)
    }
}

fn checked_call_id(id: u64) -> u64 {
    assert!(id != 0, "invalid daemon call id 0");
    id
}

fn read_ip_address(bytes: &[u8], name: &str) -> io::Result<IpAddr> {
    match bytes.len() {
        4 => Ok(IpAddr::V4(read_ipv4(bytes, name)?)),
        16 => Ok(IpAddr::V6(read_ipv6(bytes, name)?)),
        length => Err(invalid_data(format!("invalid {name} length {length}"))),
    }
}

fn read_ipv4(bytes: &[u8], name: &str) -> io::Result<Ipv4Addr> {
    let length = bytes.len();
    let raw: [u8; 4] = bytes
        .try_into()
        .map_err(|_| invalid_data(format!("invalid {name} length {length}")))?;
    Ok(Ipv4Addr::from(raw))
}

fn read_ipv6(bytes: &[u8], name: &str) -> io::Result<Ipv6Addr> {
    let length = bytes.len();
    let raw: [u8; 16] = bytes
        .try_into()
        .map_err(|_| invalid_data(format!("invalid {name} length {length}")))?;
    Ok(Ipv6Addr::from(raw))
}

fn read_mac(bytes: &[u8], name: &str) -> io::Result<[u8; 6]> {
    let length = bytes.len();
    bytes
        .try_into()
        .map_err(|_| invalid_data(format!("invalid {name} length {length}")))
}

fn read_prefix_len(prefix_len: u32, max: u32, name: &str) -> io::Result<u8> {
    if prefix_len > max {
        Err(invalid_data(format!("invalid {name} {prefix_len}")))
    } else {
        Ok(prefix_len as u8)
    }
}

fn required<T>(value: Option<T>, name: &str) -> io::Result<T> {
    value.ok_or_else(|| invalid_data(format!("missing {name}")))
}

fn invalid_decode_error(error: prost::DecodeError) -> io::Error {
    invalid_data(error.to_string())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn trim_error_field(value: String) -> String {
    if value.len() <= MAX_ERROR_FIELD_BYTES {
        return value;
    }
    let mut end = MAX_ERROR_FIELD_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut value = value[..end].to_owned();
    value.push_str("...");
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_context_preserves_existing_report() {
        let report = daemon_error_report();
        let error = io::Error::other(DaemonReportError {
            report: report.clone(),
        })
        .with_report_context("outer");

        assert_eq!(
            DaemonErrorReport::from_reported_io_error(&error),
            Some(report)
        );
    }

    fn daemon_error_report() -> DaemonErrorReport {
        DaemonErrorReport {
            context: "routing.command".to_owned(),
            message: "Device or resource busy".to_owned(),
            errno: Some(libc::EBUSY),
            kind: "ResourceBusy".to_owned(),
            file: "routing.rs".to_owned(),
            line: 123,
            column: 45,
            pid: 2345,
            details: vec![("command".to_owned(), "iptables-restore".to_owned())],
        }
    }
}
