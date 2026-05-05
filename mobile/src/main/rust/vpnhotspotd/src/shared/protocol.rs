use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::shared::model::{
    ipv6_nat_gateway, ipv6_nat_prefix, ipv6_to_u128, ClientConfig, Ipv6NatConfig, MasqueradeMode,
    Network, Route, SessionConfig, SessionPorts, UpstreamConfig, UpstreamRole, DAEMON_REPLY_MARK,
};

const STATUS_OK: u8 = 0;
const STATUS_ERROR: u8 = 1;

const FRAME_REPLY: u8 = 0;
const FRAME_NEIGHBOURS: u8 = 1;

const CMD_START_SESSION: u32 = 1;
const CMD_REPLACE_SESSION: u32 = 2;
const CMD_REMOVE_SESSION: u32 = 3;
const CMD_SHUTDOWN: u32 = 4;
const CMD_READ_TRAFFIC_COUNTERS: u32 = 5;
const CMD_START_NEIGHBOUR_MONITOR: u32 = 6;
const CMD_STOP_NEIGHBOUR_MONITOR: u32 = 7;
const CMD_DUMP_NEIGHBOURS: u32 = 8;
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
    StopNeighbourMonitor,
    DumpNeighbours,
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
    pub lladdr: Vec<u8>,
    pub state: NeighbourState,
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
        CMD_STOP_NEIGHBOUR_MONITOR => Ok(Command::StopNeighbourMonitor),
        CMD_DUMP_NEIGHBOURS => Ok(Command::DumpNeighbours),
        CMD_STATIC_ADDRESS => Ok(Command::StaticAddress(parser.read_ip_address_command()?)),
        CMD_CLEAN_ROUTING => Ok(Command::CleanRouting(parser.read_clean_ip_command()?)),
        command => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown command {command}"),
        )),
    }
}

pub fn reply_frame(packet: Vec<u8>) -> Vec<u8> {
    let mut frame = Vec::with_capacity(packet.len() + 1);
    frame.push(FRAME_REPLY);
    frame.extend_from_slice(&packet);
    frame
}

pub fn neighbours_frame(replace: bool, neighbours: &[Neighbour]) -> Vec<u8> {
    let payload = neighbours_payload(replace, neighbours);
    let mut frame = Vec::with_capacity(payload.len() + 1);
    frame.push(FRAME_NEIGHBOURS);
    frame.extend_from_slice(&payload);
    frame
}

pub fn ok_packet() -> Vec<u8> {
    vec![STATUS_OK]
}

pub fn ports_packet(ports: SessionPorts) -> Vec<u8> {
    let mut packet = vec![STATUS_OK];
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

pub fn error_packet(error: io::Error) -> Vec<u8> {
    let mut packet = vec![STATUS_ERROR];
    packet.extend_from_slice(&error.raw_os_error().unwrap_or(-1).to_be_bytes());
    let message = error.to_string().into_bytes();
    packet.extend_from_slice(&(message.len() as u32).to_be_bytes());
    packet.extend_from_slice(&message);
    packet
}

pub fn should_suppress_static_address_error(error: &io::Error, operation: IpOperation) -> bool {
    match operation {
        IpOperation::Replace => error.raw_os_error() == Some(libc::EEXIST),
        IpOperation::Delete => matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)),
    }
}

pub fn traffic_counter_lines_packet(lines: &[String]) -> Vec<u8> {
    let mut packet = vec![STATUS_OK];
    packet.extend_from_slice(&(lines.len() as u32).to_be_bytes());
    for line in lines {
        write_utf(&mut packet, line);
    }
    packet
}

pub fn neighbours_packet(neighbours: &[Neighbour]) -> Vec<u8> {
    let payload = neighbours_payload(false, neighbours);
    let mut packet = Vec::with_capacity(payload.len());
    packet.push(STATUS_OK);
    packet.extend_from_slice(&payload[1..]);
    packet
}

fn neighbours_payload(replace: bool, neighbours: &[Neighbour]) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.push(u8::from(replace));
    packet.extend_from_slice(&(neighbours.len() as u32).to_be_bytes());
    for neighbour in neighbours {
        packet.push(neighbour.state as u8);
        match neighbour.address {
            IpAddr::V4(address) => {
                packet.extend_from_slice(&4u32.to_be_bytes());
                packet.extend_from_slice(&address.octets());
            }
            IpAddr::V6(address) => {
                packet.extend_from_slice(&16u32.to_be_bytes());
                packet.extend_from_slice(&address.octets());
            }
        }
        write_utf(&mut packet, &neighbour.interface);
        packet.extend_from_slice(&(neighbour.lladdr.len() as u32).to_be_bytes());
        packet.extend_from_slice(&neighbour.lladdr);
    }
    packet
}

fn write_utf(packet: &mut Vec<u8>, value: &str) {
    packet.extend_from_slice(&(value.len() as u32).to_be_bytes());
    packet.extend_from_slice(value.as_bytes());
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
}
