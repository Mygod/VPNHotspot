use std::error::Error;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::panic::Location;
use std::process;

use crate::shared::model::{
    ipv6_nat_gateway, ipv6_nat_prefix, ipv6_to_u128, ClientConfig, Ipv6NatConfig, MasqueradeMode,
    Network, Route, SessionConfig, SessionPorts, UpstreamConfig, UpstreamRole, DAEMON_REPLY_MARK,
};

const MAX_ERROR_DETAILS: usize = 32;
const MAX_ERROR_FIELD_BYTES: usize = 4096;

const CMD_START_SESSION: u32 = 1;
const CMD_REPLACE_SESSION: u32 = 2;
const CMD_REMOVE_SESSION: u32 = 3;
const CMD_SHUTDOWN: u32 = 4;
const CMD_READ_TRAFFIC_COUNTERS: u32 = 5;
const CMD_START_NEIGHBOUR_MONITOR: u32 = 6;
const CMD_STATIC_ADDRESS: u32 = 9;
const CMD_CLEAN_ROUTING: u32 = 12;
const NETWORK_UNSPECIFIED: Network = 0;
const ROUTE_WIRE_LEN: usize = 16 + 4;

pub enum Command {
    StartSession(SessionConfig),
    ReplaceSession(SessionConfig),
    RemoveSession {
        downstream: String,
        withdraw_cleanup: bool,
    },
    Shutdown {
        withdraw_cleanup: bool,
    },
    ReadTrafficCounters,
    StartNeighbourMonitor,
    StaticAddress(IpAddressCommand),
    CleanRouting(CleanIpCommand),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NeighbourState {
    Unset = 0,
    Incomplete = 1,
    Valid = 2,
    Failed = 3,
    Deleting = 4,
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
pub enum IpOperation {
    Replace,
    Delete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpFamily {
    Ipv4,
    Ipv6,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteType {
    Unicast,
    Local,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleAction {
    Lookup,
    Unreachable,
    Any,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IpAddressCommand {
    pub operation: IpOperation,
    pub address: IpAddr,
    pub prefix_len: u8,
    pub interface: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IpRouteCommand {
    pub operation: IpOperation,
    pub route_type: RouteType,
    pub destination: IpAddr,
    pub prefix_len: u8,
    pub interface: String,
    pub table: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IpRuleCommand {
    pub operation: IpOperation,
    pub family: IpFamily,
    pub iif: String,
    pub priority: u32,
    pub action: RuleAction,
    pub table: u32,
    pub fwmark: Option<(u32, u32)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CleanIpCommand {
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

pub fn parse_command(packet: &[u8]) -> io::Result<Command> {
    let mut parser = Parser::new(packet);
    match parser.read_u32()? {
        CMD_START_SESSION => Ok(Command::StartSession(parser.read_session_config()?)),
        CMD_REPLACE_SESSION => Ok(Command::ReplaceSession(parser.read_session_config()?)),
        CMD_REMOVE_SESSION => Ok(Command::RemoveSession {
            downstream: parser.read_utf()?,
            withdraw_cleanup: parser.read_bool()?,
        }),
        CMD_SHUTDOWN => Ok(Command::Shutdown {
            withdraw_cleanup: parser.read_bool()?,
        }),
        CMD_READ_TRAFFIC_COUNTERS => Ok(Command::ReadTrafficCounters),
        CMD_START_NEIGHBOUR_MONITOR => Ok(Command::StartNeighbourMonitor),
        CMD_STATIC_ADDRESS => Ok(Command::StaticAddress(parser.read_ip_address_command()?)),
        CMD_CLEAN_ROUTING => Ok(Command::CleanRouting(parser.read_clean_ip_command()?)),
        command => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown command {command}"),
        )),
    }
}

pub fn ok_packet() -> Vec<u8> {
    Vec::new()
}

pub fn ports_packet(ports: SessionPorts) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&ports.dns_tcp.to_be_bytes());
    packet.extend_from_slice(&ports.dns_udp.to_be_bytes());
    if let Some(ipv6_nat) = ports.ipv6_nat {
        packet.push(1);
        packet.extend_from_slice(&ipv6_nat.tcp.to_be_bytes());
        packet.extend_from_slice(&ipv6_nat.udp.to_be_bytes());
    } else {
        packet.push(0);
    }
    packet
}

pub fn should_suppress_static_address_error(error: &io::Error, operation: IpOperation) -> bool {
    match operation {
        IpOperation::Replace => error_errno(error) == Some(libc::EEXIST),
        IpOperation::Delete => matches!(error_errno(error), Some(libc::ENOENT | libc::ESRCH)),
    }
}

pub fn traffic_counter_lines_packet(lines: &[String]) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&(lines.len() as u32).to_be_bytes());
    for line in lines {
        write_utf(&mut packet, line);
    }
    packet
}

pub fn neighbour_deltas_packet<I>(deltas: I) -> Vec<u8>
where
    I: IntoIterator<Item = NeighbourDelta>,
{
    let deltas: Vec<_> = deltas.into_iter().collect();
    let mut packet = Vec::new();
    packet.extend_from_slice(&(deltas.len() as u32).to_be_bytes());
    for delta in deltas {
        match delta {
            NeighbourDelta::Upsert(neighbour) => {
                packet.push(0);
                packet.push(neighbour.state as u8);
                write_ip_address(&mut packet, neighbour.address);
                write_utf(&mut packet, &neighbour.interface);
                if let Some(lladdr) = neighbour.lladdr {
                    packet.push(1);
                    packet.extend_from_slice(&lladdr);
                } else {
                    packet.push(0);
                }
            }
            NeighbourDelta::Delete { address, interface } => {
                packet.push(1);
                write_ip_address(&mut packet, address);
                write_utf(&mut packet, &interface);
            }
        }
    }
    packet
}

fn write_ip_address(packet: &mut Vec<u8>, address: IpAddr) {
    match address {
        IpAddr::V4(address) => {
            packet.extend_from_slice(&4u32.to_be_bytes());
            packet.extend_from_slice(&address.octets());
        }
        IpAddr::V6(address) => {
            packet.extend_from_slice(&16u32.to_be_bytes());
            packet.extend_from_slice(&address.octets());
        }
    }
}

fn write_utf(packet: &mut Vec<u8>, value: &str) {
    packet.extend_from_slice(&(value.len() as u32).to_be_bytes());
    packet.extend_from_slice(value.as_bytes());
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

struct Parser<'a> {
    packet: &'a [u8],
    offset: usize,
}

impl<'a> Parser<'a> {
    fn new(packet: &'a [u8]) -> Self {
        Self { packet, offset: 0 }
    }

    fn read_u32(&mut self) -> io::Result<u32> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_u64(&mut self) -> io::Result<u64> {
        let bytes = self.read_exact(8)?;
        Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_i32(&mut self) -> io::Result<i32> {
        let bytes = self.read_exact(4)?;
        Ok(i32::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        let bytes = self.read_exact(1)?;
        Ok(bytes[0])
    }

    fn read_bool(&mut self) -> io::Result<bool> {
        Ok(self.read_u8()? != 0)
    }

    fn read_utf(&mut self) -> io::Result<String> {
        let length = self.read_u32()? as usize;
        let bytes = self.read_exact(length)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utf-8"))
    }

    fn read_session_config(&mut self) -> io::Result<SessionConfig> {
        let downstream = self.read_utf()?;
        let dns_bind_address = self.read_ipv4()?;
        let downstream_prefix_len = self.read_ipv4_prefix_len()?;
        let ip_forward = self.read_bool()?;
        let forward = self.read_bool()?;
        let masquerade = self.read_masquerade_mode()?;
        let ipv6_block = self.read_bool()?;
        let primary_network = self.read_network()?;
        let primary_routes = self.read_routes()?;
        let fallback_network = self.read_network()?;
        let upstreams = self.read_upstreams()?;
        let clients = self.read_clients()?;
        let ipv6_nat = if self.read_bool()? {
            let prefix = ipv6_nat_prefix(&self.read_utf()?, &downstream);
            let gateway = ipv6_nat_gateway(prefix);
            let mtu = self.read_i32()? as u32;
            let suppressed_prefixes = self
                .read_routes()?
                .into_iter()
                .filter(|route| {
                    route.prefix != prefix.prefix || route.prefix_len != prefix.prefix_len
                })
                .collect();
            Some(Ipv6NatConfig {
                gateway,
                prefix_len: prefix.prefix_len,
                mtu,
                suppressed_prefixes,
                cleanup_prefixes: self.read_routes()?,
            })
        } else {
            None
        };
        Ok(SessionConfig {
            downstream,
            dns_bind_address,
            downstream_prefix_len,
            reply_mark: DAEMON_REPLY_MARK,
            ip_forward,
            forward,
            masquerade,
            ipv6_block,
            primary_network,
            primary_routes,
            fallback_network,
            upstreams,
            clients,
            ipv6_nat,
        })
    }

    fn read_masquerade_mode(&mut self) -> io::Result<MasqueradeMode> {
        match self.read_u8()? {
            0 => Ok(MasqueradeMode::None),
            1 => Ok(MasqueradeMode::Simple),
            2 => Ok(MasqueradeMode::Netd),
            value => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid masquerade mode {value}"),
            )),
        }
    }

    fn read_upstreams(&mut self) -> io::Result<Vec<UpstreamConfig>> {
        let count = self.read_count("upstream")?;
        let mut upstreams = Vec::with_capacity(count);
        for _ in 0..count {
            let role = match self.read_u8()? {
                0 => UpstreamRole::Primary,
                1 => UpstreamRole::Fallback,
                value => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid upstream role {value}"),
                    ));
                }
            };
            let ifname = self.read_utf()?;
            let ifindex = self.read_u32()?;
            upstreams.push(UpstreamConfig {
                ifname,
                ifindex,
                role,
            });
        }
        Ok(upstreams)
    }

    fn read_clients(&mut self) -> io::Result<Vec<ClientConfig>> {
        let count = self.read_count("client")?;
        let mut clients = Vec::with_capacity(count);
        for _ in 0..count {
            let mac = self.read_exact(6)?.try_into().unwrap();
            let address_count = self.read_count("client address")?;
            let mut ipv4 = Vec::with_capacity(address_count);
            for _ in 0..address_count {
                ipv4.push(self.read_ipv4()?);
            }
            clients.push(ClientConfig { mac, ipv4 });
        }
        Ok(clients)
    }

    fn read_ip_address_command(&mut self) -> io::Result<IpAddressCommand> {
        let operation = self.read_ip_operation()?;
        let address = self.read_ip_address()?;
        let prefix_len = self.read_prefix_len(&address)?;
        let interface = self.read_utf()?;
        Ok(IpAddressCommand {
            operation,
            address,
            prefix_len,
            interface,
        })
    }

    fn read_clean_ip_command(&mut self) -> io::Result<CleanIpCommand> {
        Ok(CleanIpCommand {
            ipv6_nat_prefix_seed: self.read_utf()?,
        })
    }

    fn read_ip_operation(&mut self) -> io::Result<IpOperation> {
        match self.read_u8()? {
            0 => Ok(IpOperation::Replace),
            1 => Ok(IpOperation::Delete),
            value => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid IP operation {value}"),
            )),
        }
    }

    fn read_network(&mut self) -> io::Result<Option<Network>> {
        match self.read_u64()? {
            NETWORK_UNSPECIFIED => Ok(None),
            network => Ok(Some(network)),
        }
    }

    fn read_routes(&mut self) -> io::Result<Vec<Route>> {
        let routes = self.read_count("route")?;
        let remaining = self.packet.len() - self.offset;
        if routes > remaining / ROUTE_WIRE_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("route count {routes} exceeds remaining packet length {remaining}"),
            ));
        }
        let mut parsed = Vec::with_capacity(routes);
        for _ in 0..routes {
            parsed.push(self.read_route()?);
        }
        Ok(parsed)
    }

    fn read_route(&mut self) -> io::Result<Route> {
        let address = self.read_ipv6()?;
        let prefix_len = self.read_ipv6_prefix_len()?;
        Ok(Route {
            prefix: ipv6_to_u128(address),
            prefix_len,
        })
    }

    fn read_ipv4(&mut self) -> io::Result<Ipv4Addr> {
        let bytes: [u8; 4] = self.read_exact(4)?.try_into().unwrap();
        Ok(Ipv4Addr::from(bytes))
    }

    fn read_ipv6(&mut self) -> io::Result<Ipv6Addr> {
        let bytes: [u8; 16] = self.read_exact(16)?.try_into().unwrap();
        Ok(Ipv6Addr::from(bytes))
    }

    fn read_ipv6_prefix_len(&mut self) -> io::Result<u8> {
        let prefix_len = self.read_i32()?;
        if !(0..=128).contains(&prefix_len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid IPv6 prefix length {prefix_len}"),
            ));
        }
        Ok(prefix_len as u8)
    }

    fn read_ipv4_prefix_len(&mut self) -> io::Result<u8> {
        let prefix_len = self.read_i32()?;
        if !(0..=32).contains(&prefix_len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid IPv4 prefix length {prefix_len}"),
            ));
        }
        Ok(prefix_len as u8)
    }

    fn read_ip_address(&mut self) -> io::Result<IpAddr> {
        let length = self.read_u32()?;
        let bytes = self.read_exact(length as usize)?;
        match length {
            4 => Ok(IpAddr::V4(Ipv4Addr::from(
                <[u8; 4]>::try_from(bytes).unwrap(),
            ))),
            16 => Ok(IpAddr::V6(Ipv6Addr::from(
                <[u8; 16]>::try_from(bytes).unwrap(),
            ))),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid IP address length {length}"),
            )),
        }
    }

    fn read_prefix_len(&mut self, address: &IpAddr) -> io::Result<u8> {
        let prefix_len = self.read_i32()?;
        let max = match address {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if !(0..=max).contains(&prefix_len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid IP prefix length {prefix_len}"),
            ));
        }
        Ok(prefix_len as u8)
    }

    fn read_exact(&mut self, count: usize) -> io::Result<&'a [u8]> {
        if self.offset + count > self.packet.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short packet"));
        }
        let bytes = &self.packet[self.offset..self.offset + count];
        self.offset += count;
        Ok(bytes)
    }

    fn read_count(&mut self, name: &str) -> io::Result<usize> {
        let count = self.read_i32()?;
        if count < 0 {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid {name} count {count}"),
            ))
        } else {
            Ok(count as usize)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn parse_session_config_reads_networks_and_primary_routes() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_START_SESSION.to_be_bytes());
        write_utf(&mut packet, "wlan0");
        packet.extend_from_slice(&Ipv4Addr::new(192, 0, 2, 1).octets());
        write_session_flags(&mut packet);
        packet.extend_from_slice(&123u64.to_be_bytes());
        packet.extend_from_slice(&2i32.to_be_bytes());
        write_route(&mut packet, "2001:db8::".parse().unwrap(), 32);
        write_route(&mut packet, "fd00::".parse().unwrap(), 8);
        packet.extend_from_slice(&456u64.to_be_bytes());
        packet.extend_from_slice(&1i32.to_be_bytes());
        packet.push(0);
        write_utf(&mut packet, "rmnet_data0");
        packet.extend_from_slice(&1234u32.to_be_bytes());
        packet.extend_from_slice(&1i32.to_be_bytes());
        packet.extend_from_slice(&[0x02, 0, 0, 0, 0, 1]);
        packet.extend_from_slice(&1i32.to_be_bytes());
        packet.extend_from_slice(&Ipv4Addr::new(192, 0, 2, 10).octets());
        packet.push(0);

        let Command::StartSession(config) = parse_command(&packet).unwrap() else {
            panic!("expected start session");
        };
        assert_eq!(config.downstream, "wlan0");
        assert_eq!(config.dns_bind_address, Ipv4Addr::new(192, 0, 2, 1));
        assert_eq!(config.downstream_prefix_len, 24);
        assert_eq!(config.reply_mark, DAEMON_REPLY_MARK);
        assert!(config.forward);
        assert_eq!(config.masquerade, MasqueradeMode::Simple);
        assert_eq!(config.primary_network, Some(123));
        assert_eq!(config.primary_routes.len(), 2);
        assert_eq!(config.primary_routes[0].prefix_len, 32);
        assert_eq!(config.primary_routes[1].prefix_len, 8);
        assert_eq!(config.fallback_network, Some(456));
        assert_eq!(config.upstreams.len(), 1);
        assert_eq!(config.upstreams[0].role, UpstreamRole::Primary);
        assert_eq!(config.upstreams[0].ifname, "rmnet_data0");
        assert_eq!(config.upstreams[0].ifindex, 1234);
        assert_eq!(config.clients.len(), 1);
        assert_eq!(config.clients[0].mac, [0x02, 0, 0, 0, 0, 1]);
        assert_eq!(config.clients[0].ipv4, vec![Ipv4Addr::new(192, 0, 2, 10)]);
        assert!(config.ipv6_nat.is_none());
    }

    #[test]
    fn parse_session_config_maps_zero_networks_to_none() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_REPLACE_SESSION.to_be_bytes());
        write_utf(&mut packet, "wlan0");
        packet.extend_from_slice(&Ipv4Addr::new(192, 0, 2, 1).octets());
        write_session_flags(&mut packet);
        packet.extend_from_slice(&0u64.to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes());
        packet.extend_from_slice(&0u64.to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes());
        packet.push(0);

        let Command::ReplaceSession(config) = parse_command(&packet).unwrap() else {
            panic!("expected replace session");
        };
        assert_eq!(config.primary_network, None);
        assert!(config.primary_routes.is_empty());
        assert_eq!(config.fallback_network, None);
    }

    #[test]
    fn parse_session_config_derives_ipv6_nat_prefix() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_START_SESSION.to_be_bytes());
        write_utf(&mut packet, "wlan0");
        packet.extend_from_slice(&Ipv4Addr::new(192, 0, 2, 1).octets());
        write_session_flags(&mut packet);
        packet.extend_from_slice(&0u64.to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes());
        packet.extend_from_slice(&0u64.to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes());
        packet.push(1);
        write_utf(&mut packet, "be.mygod.vpnhotspot\0android-id");
        packet.extend_from_slice(&1280i32.to_be_bytes());
        packet.extend_from_slice(&2i32.to_be_bytes());
        write_route(&mut packet, "fd8d:32f9:31e3:b417::".parse().unwrap(), 64);
        write_route(&mut packet, "2001:db8::".parse().unwrap(), 32);
        packet.extend_from_slice(&0i32.to_be_bytes());

        let Command::StartSession(config) = parse_command(&packet).unwrap() else {
            panic!("expected start session");
        };
        let ipv6_nat = config.ipv6_nat.unwrap();
        assert_eq!(
            ipv6_nat.gateway,
            "fd8d:32f9:31e3:b417::1".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(ipv6_nat.prefix_len, 64);
        assert_eq!(ipv6_nat.mtu, 1280);
        assert_eq!(ipv6_nat.suppressed_prefixes, vec![route("2001:db8::", 32)]);
    }

    #[test]
    fn parse_session_config_rejects_negative_route_count() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_START_SESSION.to_be_bytes());
        write_utf(&mut packet, "wlan0");
        packet.extend_from_slice(&Ipv4Addr::new(192, 0, 2, 1).octets());
        write_session_flags(&mut packet);
        packet.extend_from_slice(&123u64.to_be_bytes());
        packet.extend_from_slice(&(-1i32).to_be_bytes());

        let error = match parse_command(&packet) {
            Err(error) => error,
            Ok(_) => panic!("expected route count failure"),
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "invalid route count -1");
    }

    #[test]
    fn parse_session_config_rejects_route_count_exceeding_remaining_packet() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_START_SESSION.to_be_bytes());
        write_utf(&mut packet, "wlan0");
        packet.extend_from_slice(&Ipv4Addr::new(192, 0, 2, 1).octets());
        write_session_flags(&mut packet);
        packet.extend_from_slice(&123u64.to_be_bytes());
        packet.extend_from_slice(&1i32.to_be_bytes());

        let error = match parse_command(&packet) {
            Err(error) => error,
            Ok(_) => panic!("expected route count failure"),
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "route count 1 exceeds remaining packet length 0"
        );
    }

    #[test]
    fn static_address_error_suppression_matches_idempotent_address_ops() {
        assert!(should_suppress_static_address_error(
            &io::Error::from_raw_os_error(libc::EEXIST),
            IpOperation::Replace,
        ));
        assert!(!should_suppress_static_address_error(
            &io::Error::from_raw_os_error(libc::ENOENT),
            IpOperation::Replace,
        ));
        assert!(should_suppress_static_address_error(
            &io::Error::from_raw_os_error(libc::ENOENT),
            IpOperation::Delete,
        ));
        assert!(should_suppress_static_address_error(
            &io::Error::from_raw_os_error(libc::ESRCH),
            IpOperation::Delete,
        ));
        assert!(!should_suppress_static_address_error(
            &io::Error::from_raw_os_error(libc::EEXIST),
            IpOperation::Delete,
        ));
    }

    #[test]
    fn static_address_error_suppression_preserves_reported_errno() {
        let existing = io::Error::from_raw_os_error(libc::EEXIST).with_report_context("address");
        assert_eq!(existing.raw_os_error(), None);
        assert_eq!(error_errno(&existing), Some(libc::EEXIST));
        assert!(should_suppress_static_address_error(
            &existing,
            IpOperation::Replace,
        ));

        let missing = io::Error::from_raw_os_error(libc::ENOENT).with_report_context("address");
        assert_eq!(missing.raw_os_error(), None);
        assert_eq!(error_errno(&missing), Some(libc::ENOENT));
        assert!(should_suppress_static_address_error(
            &missing,
            IpOperation::Delete,
        ));
    }

    #[test]
    fn parse_clean_routing_reads_prefix_seed() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_CLEAN_ROUTING.to_be_bytes());
        write_utf(&mut packet, "be.mygod.vpnhotspot\0android-id");

        let Command::CleanRouting(command) = parse_command(&packet).unwrap() else {
            panic!("expected clean routing");
        };
        assert_eq!(
            command.ipv6_nat_prefix_seed,
            "be.mygod.vpnhotspot\0android-id"
        );
    }

    #[test]
    fn neighbour_deltas_packet_encodes_empty_list() {
        let packet = neighbour_deltas_packet(std::iter::empty::<NeighbourDelta>());
        let mut parser = Parser::new(&packet);

        assert_eq!(parser.read_u32().unwrap(), 0);
        assert_eq!(packet.len(), 4);
    }

    #[test]
    fn neighbour_deltas_packet_encodes_upsert_and_delete() {
        let packet = neighbour_deltas_packet([
            NeighbourDelta::Upsert(Neighbour {
                address: "192.0.2.2".parse().unwrap(),
                interface: "wlan0".to_owned(),
                lladdr: Some([2, 0, 0, 0, 0, 1]),
                state: NeighbourState::Valid,
            }),
            NeighbourDelta::Delete {
                address: "2001:db8::1".parse().unwrap(),
                interface: "wlan1".to_owned(),
            },
        ]);
        let mut parser = Parser::new(&packet);

        assert_eq!(parser.read_u32().unwrap(), 2);
        assert_eq!(parser.read_u8().unwrap(), 0);
        assert_eq!(parser.read_u8().unwrap(), NeighbourState::Valid as u8);
        assert_eq!(
            parser.read_ip_address().unwrap(),
            "192.0.2.2".parse::<IpAddr>().unwrap()
        );
        assert_eq!(parser.read_utf().unwrap(), "wlan0");
        assert_eq!(parser.read_u8().unwrap(), 1);
        assert_eq!(parser.read_exact(6).unwrap(), &[2, 0, 0, 0, 0, 1]);
        assert_eq!(parser.read_u8().unwrap(), 1);
        assert_eq!(
            parser.read_ip_address().unwrap(),
            "2001:db8::1".parse::<IpAddr>().unwrap()
        );
        assert_eq!(parser.read_utf().unwrap(), "wlan1");
    }

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

    fn write_utf(packet: &mut Vec<u8>, value: &str) {
        packet.extend_from_slice(&(value.len() as u32).to_be_bytes());
        packet.extend_from_slice(value.as_bytes());
    }

    fn write_session_flags(packet: &mut Vec<u8>) {
        packet.extend_from_slice(&24i32.to_be_bytes());
        packet.push(0);
        packet.push(1);
        packet.push(1);
        packet.push(0);
    }

    fn write_route(packet: &mut Vec<u8>, address: Ipv6Addr, prefix_len: i32) {
        packet.extend_from_slice(&address.octets());
        packet.extend_from_slice(&prefix_len.to_be_bytes());
    }

    fn route(address: &str, prefix_len: u8) -> Route {
        Route {
            prefix: ipv6_to_u128(address.parse::<Ipv6Addr>().unwrap()),
            prefix_len,
        }
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
