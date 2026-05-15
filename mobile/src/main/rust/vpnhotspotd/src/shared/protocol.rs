use std::error::Error;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::panic::Location;
use std::process;

use cidr::{Ipv6Cidr, Ipv6Inet};
use prost::Message;

use crate::shared::model::{
    ipv6_nat_gateway, ipv6_nat_prefix, ClientConfig, Ipv6NatConfig, SessionConfig,
    DAEMON_REPLY_MARK,
};
use crate::shared::proto::daemon::{self, DaemonErrorReport, MasqueradeMode};

const MAX_ERROR_DETAILS: usize = 32;
const MAX_ERROR_FIELD_BYTES: usize = 4096;

#[track_caller]
pub fn daemon_error_report(
    context: impl Into<String>,
    message: impl Into<String>,
    kind: impl Into<String>,
) -> DaemonErrorReport {
    daemon_error_report_with_details(context, message, kind, std::iter::empty::<(&str, &str)>())
}

#[track_caller]
pub fn daemon_error_report_with_details<I, K, V>(
    context: impl Into<String>,
    message: impl Into<String>,
    kind: impl Into<String>,
    details: I,
) -> DaemonErrorReport
where
    I: IntoIterator<Item = (K, V)>,
    K: ToString,
    V: ToString,
{
    let location = Location::caller();
    daemon::DaemonErrorReport {
        context: trim_error_field(context.into()),
        message: trim_error_field(message.into()),
        errno: None,
        kind: trim_error_field(kind.into()),
        file: trim_error_field(location.file().to_owned()),
        line: location.line(),
        column: location.column(),
        pid: process::id(),
        details: error_details(details),
    }
}

#[track_caller]
pub fn daemon_io_error_report(context: impl Into<String>, error: io::Error) -> DaemonErrorReport {
    daemon_io_error_report_with_details(context, error, std::iter::empty::<(&str, &str)>())
}

#[track_caller]
pub fn daemon_io_error_report_with_details<I, K, V>(
    context: impl Into<String>,
    error: io::Error,
    details: I,
) -> DaemonErrorReport
where
    I: IntoIterator<Item = (K, V)>,
    K: ToString,
    V: ToString,
{
    if let Some(report) = reported_io_error_report(&error) {
        return report;
    }
    let location = Location::caller();
    daemon::DaemonErrorReport {
        context: trim_error_field(context.into()),
        message: trim_error_field(error.to_string()),
        errno: error.raw_os_error(),
        kind: trim_error_field(format!("{:?}", error.kind())),
        file: trim_error_field(location.file().to_owned()),
        line: location.line(),
        column: location.column(),
        pid: process::id(),
        details: error_details(details),
    }
}

fn reported_io_error_report(error: &io::Error) -> Option<DaemonErrorReport> {
    error
        .get_ref()?
        .downcast_ref::<DaemonReportError>()
        .map(|error| error.report.clone())
}

fn error_details<I, K, V>(details: I) -> Vec<daemon::ErrorDetail>
where
    I: IntoIterator<Item = (K, V)>,
    K: ToString,
    V: ToString,
{
    details
        .into_iter()
        .take(MAX_ERROR_DETAILS)
        .map(|(key, value)| daemon::ErrorDetail {
            key: trim_error_field(key.to_string()),
            value: trim_error_field(value.to_string()),
        })
        .collect()
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
    reported_io_error_report(error)
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
        if reported_io_error_report(&self).is_some() {
            return self;
        }
        let kind = self.kind();
        let report = daemon_io_error_report_with_details(context, self, details);
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

pub fn neighbour_monitor_update_frame<I>(
    id: u64,
    neighbour_deltas: I,
    link_topology: Option<daemon::LinkTopologySnapshot>,
) -> Vec<u8>
where
    I: IntoIterator<Item = daemon::NeighbourDelta>,
{
    event_frame(
        id,
        daemon::event_frame::Payload::NeighbourMonitor(daemon::NeighbourMonitorUpdate {
            neighbour_deltas: neighbour_deltas.into_iter().collect(),
            link_topology,
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
        report: Some(report),
    }))
}

pub fn nonfatal_frame(id: Option<u64>, report: DaemonErrorReport) -> Vec<u8> {
    daemon_envelope(daemon::daemon_envelope::Frame::NonFatal(
        daemon::NonFatalFrame {
            call_id: id.map(checked_call_id),
            report: Some(report),
        },
    ))
}

pub fn read_session_config(config: daemon::SessionConfig) -> io::Result<SessionConfig> {
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
        masquerade: MasqueradeMode::try_from(config.masquerade)
            .map_err(|_| invalid_data(format!("invalid masquerade mode {}", config.masquerade)))?,
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

pub fn read_ip_address_entry(entry: &daemon::IpAddressEntry) -> io::Result<(IpAddr, u8)> {
    let ip = read_ip_address(&entry.address, "static address")?;
    let prefix_len = read_prefix_len(
        entry.prefix_length,
        match ip {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        },
        "static address prefix length",
    )?;
    Ok((ip, prefix_len))
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

        assert_eq!(reported_io_error_report(&error), Some(report));
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
            details: vec![daemon::ErrorDetail {
                key: "command".to_owned(),
                value: "iptables-restore".to_owned(),
            }],
        }
    }
}
