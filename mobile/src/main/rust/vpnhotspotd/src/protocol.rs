use std::io;
use std::net::Ipv6Addr;

use crate::model::{ipv6_to_u128, Ipv6NatConfig, Route, SessionConfig, SessionPorts, Upstream};

const STATUS_OK: u8 = 0;
const STATUS_ERROR: u8 = 1;

const CMD_START_SESSION: u32 = 1;
const CMD_REPLACE_SESSION: u32 = 2;
const CMD_REMOVE_SESSION: u32 = 3;
const CMD_SHUTDOWN: u32 = 4;

pub(crate) enum Command {
    StartSession(SessionConfig),
    ReplaceSession(SessionConfig),
    RemoveSession {
        session_id: String,
        withdraw_cleanup: bool,
    },
    Shutdown {
        withdraw_cleanup: bool,
    },
}

pub(crate) fn parse_command(packet: &[u8]) -> io::Result<Command> {
    let mut parser = Parser::new(packet);
    match parser.read_u32()? {
        CMD_START_SESSION => Ok(Command::StartSession(parser.read_session_config()?)),
        CMD_REPLACE_SESSION => Ok(Command::ReplaceSession(parser.read_session_config()?)),
        CMD_REMOVE_SESSION => Ok(Command::RemoveSession {
            session_id: parser.read_utf()?,
            withdraw_cleanup: parser.read_bool()?,
        }),
        CMD_SHUTDOWN => Ok(Command::Shutdown {
            withdraw_cleanup: parser.read_bool()?,
        }),
        command => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown command {command}"),
        )),
    }
}

pub(crate) fn ok_packet() -> Vec<u8> {
    vec![STATUS_OK]
}

pub(crate) fn ports_packet(ports: SessionPorts) -> Vec<u8> {
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

pub(crate) fn error_packet(error: io::Error) -> Vec<u8> {
    let mut packet = vec![STATUS_ERROR];
    let message = error.to_string().into_bytes();
    packet.extend_from_slice(&(message.len() as u16).to_be_bytes());
    packet.extend_from_slice(&message);
    packet
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
        Ok(self.read_u32()? as i32)
    }

    fn read_u16(&mut self) -> io::Result<u16> {
        let bytes = self.read_exact(2)?;
        Ok(u16::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        let bytes = self.read_exact(1)?;
        Ok(bytes[0])
    }

    fn read_bool(&mut self) -> io::Result<bool> {
        Ok(self.read_u8()? != 0)
    }

    fn read_utf(&mut self) -> io::Result<String> {
        let length = self.read_u16()? as usize;
        let bytes = self.read_exact(length)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utf-8"))
    }

    fn read_session_config(&mut self) -> io::Result<SessionConfig> {
        let session_id = self.read_utf()?;
        let downstream = self.read_utf()?;
        let dns_bind_address = self
            .read_utf()?
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid DNS bind address"))?;
        let reply_mark = self.read_u32()?;
        let primary = self.read_upstream()?;
        let fallback = self.read_upstream()?;
        let ipv6_nat = if self.read_bool()? {
            Some(Ipv6NatConfig {
                router: self.read_utf()?.parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid router address")
                })?,
                gateway: self.read_utf()?.parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid gateway address")
                })?,
                prefix_len: self.read_i32()? as u8,
                mtu: self.read_i32()? as u32,
                suppressed_prefixes: self.read_routes()?,
                cleanup_prefixes: self.read_routes()?,
            })
        } else {
            None
        };
        Ok(SessionConfig {
            session_id,
            downstream,
            dns_bind_address,
            reply_mark,
            primary,
            fallback,
            ipv6_nat,
        })
    }

    fn read_upstream(&mut self) -> io::Result<Option<Upstream>> {
        if !self.read_bool()? {
            return Ok(None);
        }
        let network_handle = self.read_u64()?;
        let interface = self.read_utf()?;
        Ok(Some(Upstream {
            network_handle,
            interface,
            routes: self.read_routes()?,
        }))
    }

    fn read_routes(&mut self) -> io::Result<Vec<Route>> {
        let routes = self.read_i32()? as usize;
        let mut parsed = Vec::with_capacity(routes);
        for _ in 0..routes {
            let address: Ipv6Addr = self
                .read_utf()?
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid route address"))?;
            let prefix_len = self.read_i32()? as u8;
            parsed.push(Route {
                prefix: ipv6_to_u128(address),
                prefix_len,
            });
        }
        Ok(parsed)
    }

    fn read_exact(&mut self, count: usize) -> io::Result<&'a [u8]> {
        if self.offset + count > self.packet.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short packet"));
        }
        let bytes = &self.packet[self.offset..self.offset + count];
        self.offset += count;
        Ok(bytes)
    }
}
