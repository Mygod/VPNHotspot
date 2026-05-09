use std::error::Error;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::panic::Location;
use std::process;

use crate::shared::model::{
    ipv6_nat_gateway, ipv6_nat_prefix, ipv6_to_u128, ClientConfig, Ipv6NatConfig, MasqueradeMode,
    Network, Route, SessionConfig, UpstreamConfig, UpstreamRole, DAEMON_REPLY_MARK,
};

const MAX_ERROR_DETAILS: usize = 32;
const MAX_ERROR_FIELD_BYTES: usize = 4096;

const CMD_CANCEL: u32 = 0;
const CMD_START_SESSION: u32 = 1;
const CMD_REPLACE_SESSION: u32 = 2;
const CMD_READ_TRAFFIC_COUNTERS: u32 = 5;
const CMD_START_NEIGHBOUR_MONITOR: u32 = 6;
const CMD_CLEAN_ROUTING: u32 = 12;
const CMD_REPLACE_STATIC_ADDRESSES: u32 = 13;
const CMD_DELETE_STATIC_ADDRESSES: u32 = 14;
const NETWORK_UNSPECIFIED: Network = 0;
const ROUTE_WIRE_LEN: usize = 16 + 4;

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

pub fn parse_command(packet: &[u8]) -> io::Result<Command> {
    let mut parser = Parser::new(packet);
    match parser.read_u32()? {
        CMD_CANCEL => {
            if !parser.remaining().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cancel command has payload",
                ));
            }
            Ok(Command::Cancel)
        }
        CMD_START_SESSION => Ok(Command::StartSession(parser.read_session_config()?)),
        CMD_REPLACE_SESSION => Ok(Command::ReplaceSession {
            session_id: parser.read_u64()?,
            config: parser.read_session_config()?,
        }),
        CMD_READ_TRAFFIC_COUNTERS => Ok(Command::ReadTrafficCounters),
        CMD_START_NEIGHBOUR_MONITOR => Ok(Command::StartNeighbourMonitor),
        CMD_CLEAN_ROUTING => Ok(Command::CleanRouting(parser.read_clean_routing_command()?)),
        CMD_REPLACE_STATIC_ADDRESSES => Ok(Command::ReplaceStaticAddresses(
            parser.read_static_addresses_command()?,
        )),
        CMD_DELETE_STATIC_ADDRESSES => Ok(Command::DeleteStaticAddresses {
            interface: parser.read_utf()?,
        }),
        command => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown command {command}"),
        )),
    }
}

pub fn ok_packet() -> Vec<u8> {
    Vec::new()
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
        let ip_forward = self.read_bool()?;
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
            Some(Ipv6NatConfig {
                gateway,
                prefix_len: prefix.prefix_len,
            })
        } else {
            None
        };
        Ok(SessionConfig {
            downstream,
            reply_mark: DAEMON_REPLY_MARK,
            ip_forward,
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
            upstreams.push(UpstreamConfig { ifname, role });
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

    fn read_clean_routing_command(&mut self) -> io::Result<CleanRoutingCommand> {
        Ok(CleanRoutingCommand {
            ipv6_nat_prefix_seed: self.read_utf()?,
        })
    }

    fn read_static_addresses_command(&mut self) -> io::Result<StaticAddressesCommand> {
        let interface = self.read_utf()?;
        let count = self.read_count("static address")?;
        let mut addresses = Vec::with_capacity(count);
        for _ in 0..count {
            let address = self.read_ip_address()?;
            let prefix_len = self.read_prefix_len(&address)?;
            addresses.push(IpAddressEntry {
                address,
                prefix_len,
            });
        }
        Ok(StaticAddressesCommand {
            interface,
            addresses,
        })
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

    fn remaining(&self) -> &'a [u8] {
        &self.packet[self.offset..]
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
    fn parse_cancel_command_rejects_payload() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_CANCEL.to_be_bytes());

        let Command::Cancel = parse_command(&packet).unwrap() else {
            panic!("expected cancel");
        };
        packet.push(1);
        let error = match parse_command(&packet) {
            Err(error) => error,
            Ok(_) => panic!("expected cancel payload failure"),
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "cancel command has payload");
    }

    #[test]
    fn parse_session_config_reads_networks_and_primary_routes() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_START_SESSION.to_be_bytes());
        write_utf(&mut packet, "wlan0");
        write_session_flags(&mut packet);
        packet.extend_from_slice(&123u64.to_be_bytes());
        packet.extend_from_slice(&2i32.to_be_bytes());
        write_route(&mut packet, "2001:db8::".parse().unwrap(), 32);
        write_route(&mut packet, "fd00::".parse().unwrap(), 8);
        packet.extend_from_slice(&456u64.to_be_bytes());
        packet.extend_from_slice(&1i32.to_be_bytes());
        packet.push(0);
        write_utf(&mut packet, "rmnet_data0");
        packet.extend_from_slice(&1i32.to_be_bytes());
        packet.extend_from_slice(&[0x02, 0, 0, 0, 0, 1]);
        packet.extend_from_slice(&1i32.to_be_bytes());
        packet.extend_from_slice(&Ipv4Addr::new(192, 0, 2, 10).octets());
        packet.push(0);

        let Command::StartSession(config) = parse_command(&packet).unwrap() else {
            panic!("expected start session");
        };
        assert_eq!(config.downstream, "wlan0");
        assert_eq!(config.reply_mark, DAEMON_REPLY_MARK);
        assert_eq!(config.masquerade, MasqueradeMode::Simple);
        assert_eq!(config.primary_network, Some(123));
        assert_eq!(config.primary_routes.len(), 2);
        assert_eq!(config.primary_routes[0].prefix_len, 32);
        assert_eq!(config.primary_routes[1].prefix_len, 8);
        assert_eq!(config.fallback_network, Some(456));
        assert_eq!(config.upstreams.len(), 1);
        assert_eq!(config.upstreams[0].role, UpstreamRole::Primary);
        assert_eq!(config.upstreams[0].ifname, "rmnet_data0");
        assert_eq!(config.clients.len(), 1);
        assert_eq!(config.clients[0].mac, [0x02, 0, 0, 0, 0, 1]);
        assert_eq!(config.clients[0].ipv4, vec![Ipv4Addr::new(192, 0, 2, 10)]);
        assert!(config.ipv6_nat.is_none());
    }

    #[test]
    fn parse_session_config_maps_zero_networks_to_none() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_REPLACE_SESSION.to_be_bytes());
        packet.extend_from_slice(&42u64.to_be_bytes());
        write_utf(&mut packet, "wlan0");
        write_session_flags(&mut packet);
        packet.extend_from_slice(&0u64.to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes());
        packet.extend_from_slice(&0u64.to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes());
        packet.push(0);

        let Command::ReplaceSession { session_id, config } = parse_command(&packet).unwrap() else {
            panic!("expected replace session");
        };
        assert_eq!(session_id, 42);
        assert_eq!(config.primary_network, None);
        assert!(config.primary_routes.is_empty());
        assert_eq!(config.fallback_network, None);
    }

    #[test]
    fn parse_session_config_derives_ipv6_nat_prefix() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_START_SESSION.to_be_bytes());
        write_utf(&mut packet, "wlan0");
        write_session_flags(&mut packet);
        packet.extend_from_slice(&0u64.to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes());
        packet.extend_from_slice(&0u64.to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes());
        packet.extend_from_slice(&0i32.to_be_bytes());
        packet.push(1);
        write_utf(&mut packet, "be.mygod.vpnhotspot\0android-id");

        let Command::StartSession(config) = parse_command(&packet).unwrap() else {
            panic!("expected start session");
        };
        let ipv6_nat = config.ipv6_nat.unwrap();
        assert_eq!(
            ipv6_nat.gateway,
            "fd8d:32f9:31e3:b417::1".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(ipv6_nat.prefix_len, 64);
    }

    #[test]
    fn parse_session_config_rejects_negative_route_count() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_START_SESSION.to_be_bytes());
        write_utf(&mut packet, "wlan0");
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
    fn parse_replace_static_addresses_reads_batch() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_REPLACE_STATIC_ADDRESSES.to_be_bytes());
        write_utf(&mut packet, "lo");
        packet.extend_from_slice(&2i32.to_be_bytes());
        write_ip_address(&mut packet, "192.0.2.1".parse().unwrap());
        packet.extend_from_slice(&32i32.to_be_bytes());
        write_ip_address(&mut packet, "2001:db8::1".parse().unwrap());
        packet.extend_from_slice(&128i32.to_be_bytes());

        let Command::ReplaceStaticAddresses(command) = parse_command(&packet).unwrap() else {
            panic!("expected replace static addresses");
        };
        assert_eq!(command.interface, "lo");
        assert_eq!(
            command.addresses,
            vec![
                IpAddressEntry {
                    address: "192.0.2.1".parse().unwrap(),
                    prefix_len: 32,
                },
                IpAddressEntry {
                    address: "2001:db8::1".parse().unwrap(),
                    prefix_len: 128,
                },
            ]
        );
    }

    #[test]
    fn parse_replace_static_addresses_rejects_negative_count() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_REPLACE_STATIC_ADDRESSES.to_be_bytes());
        write_utf(&mut packet, "lo");
        packet.extend_from_slice(&(-1i32).to_be_bytes());

        let error = match parse_command(&packet) {
            Err(error) => error,
            Ok(_) => panic!("expected static address count failure"),
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "invalid static address count -1");
    }

    #[test]
    fn parse_delete_static_addresses_reads_interface() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&CMD_DELETE_STATIC_ADDRESSES.to_be_bytes());
        write_utf(&mut packet, "lo");

        let Command::DeleteStaticAddresses { interface } = parse_command(&packet).unwrap() else {
            panic!("expected delete static addresses");
        };
        assert_eq!(interface, "lo");
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
        packet.push(0);
        packet.push(1);
        packet.push(0);
    }

    fn write_route(packet: &mut Vec<u8>, address: Ipv6Addr, prefix_len: i32) {
        packet.extend_from_slice(&address.octets());
        packet.extend_from_slice(&prefix_len.to_be_bytes());
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
