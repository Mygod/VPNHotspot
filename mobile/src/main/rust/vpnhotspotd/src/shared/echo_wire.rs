use std::net::IpAddr;
#[cfg(test)]
use std::net::{Ipv4Addr, Ipv6Addr};

use etherparse::{
    icmpv4, icmpv6, IcmpEchoHeader, Icmpv4Header, Icmpv4Type, Icmpv6Header, Icmpv6Type,
    IpFragOffset, IpNumber, Ipv4Header, Ipv6Header,
};

use crate::shared::ip_wire::{ipv6_payload, Ipv6Payload, Packet};
use crate::shared::packet_writer::WriterError;
use crate::shared::udp_wire::Reject;

pub const ECHO_HEADER_LEN: usize = 8;

/// Which IP family an Echo socket, session or error belongs to. Named rather than a positional `bool`,
/// because it keys tables where getting the two the wrong way round matches one family's traffic against the
/// other's - see [crate::shared::echo_session::Sessions].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    pub fn of(address: IpAddr) -> Self {
        if address.is_ipv6() {
            Self::V6
        } else {
            Self::V4
        }
    }

    pub fn ipv6(self) -> bool {
        self == Self::V6
    }
}

impl std::fmt::Display for Family {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::V4 => "IPv4",
            Self::V6 => "IPv6",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request<'a> {
    pub client: IpAddr,
    pub remote: IpAddr,
    pub hop_limit: u8,
    pub dont_fragment: bool,
    pub identity: Identity,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub identifier: u16,
    pub sequence: u16,
}

pub fn parse(packet: &[u8]) -> Result<Request<'_>, Reject> {
    match Packet::parse(packet).map_err(|error| Reject::Malformed(error.message()))? {
        Packet::Ipv4 { header, payload } => {
            if header.protocol() != IpNumber::ICMP {
                return Err(Reject::NotUdp);
            }
            if header.more_fragments() || header.fragments_offset() != IpFragOffset::ZERO {
                return Err(Reject::Fragmented);
            }
            let echo = message(payload, icmpv4::TYPE_ECHO_REQUEST)?;
            if Icmpv4Header::with_checksum(Icmpv4Type::EchoRequest(echo.header()), echo.payload)
                .checksum
                != echo.checksum
            {
                return Err(Reject::Malformed("ICMPv4 echo checksum mismatch"));
            }
            Ok(Request {
                client: IpAddr::V4(header.source_addr()),
                remote: IpAddr::V4(header.destination_addr()),
                hop_limit: header.ttl(),
                dont_fragment: header.dont_fragment(),
                identity: echo.identity(),
                payload: echo.payload,
            })
        }
        Packet::Ipv6 { header, payload } => {
            match ipv6_payload(header.next_header(), IpNumber::IPV6_ICMP) {
                Ipv6Payload::Transport => {}
                Ipv6Payload::Fragment => return Err(Reject::Fragmented),
                Ipv6Payload::Extension => return Err(Reject::Extended),
                Ipv6Payload::Other => return Err(Reject::NotUdp),
            }
            let client = header.source_addr();
            let remote = header.destination_addr();
            let echo = message(payload, icmpv6::TYPE_ECHO_REQUEST)?;
            if Icmpv6Header::with_checksum(
                Icmpv6Type::EchoRequest(echo.header()),
                client.octets(),
                remote.octets(),
                echo.payload,
            )
            .map_err(|_| Reject::Malformed("ICMPv6 echo payload too long"))?
            .checksum
                != echo.checksum
            {
                return Err(Reject::Malformed("ICMPv6 echo checksum mismatch"));
            }
            Ok(Request {
                client: IpAddr::V6(client),
                remote: IpAddr::V6(remote),
                hop_limit: header.hop_limit(),
                dont_fragment: true,
                identity: echo.identity(),
                payload: echo.payload,
            })
        }
    }
}

pub fn peek_reply(message: &[u8], ipv6: bool) -> Result<(Identity, &[u8]), Reject> {
    peek(
        message,
        if ipv6 {
            icmpv6::TYPE_ECHO_REPLY
        } else {
            icmpv4::TYPE_ECHO_REPLY
        },
    )
}

pub fn peek_request(message: &[u8], ipv6: bool) -> Result<(Identity, &[u8]), Reject> {
    peek(
        message,
        if ipv6 {
            icmpv6::TYPE_ECHO_REQUEST
        } else {
            icmpv4::TYPE_ECHO_REQUEST
        },
    )
}

fn peek(message: &[u8], expected: u8) -> Result<(Identity, &[u8]), Reject> {
    if message.len() < ECHO_HEADER_LEN {
        return Err(Reject::Malformed("ICMP echo header does not fit"));
    }
    if message[0] != expected || message[1] != 0 {
        return Err(Reject::NotUdp);
    }
    Ok((
        Identity {
            identifier: u16::from_be_bytes([message[4], message[5]]),
            sequence: u16::from_be_bytes([message[6], message[7]]),
        },
        &message[ECHO_HEADER_LEN..],
    ))
}

pub fn build_request(ipv6: bool, sequence: u16, payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(ECHO_HEADER_LEN + payload.len());
    message.extend([
        if ipv6 {
            icmpv6::TYPE_ECHO_REQUEST
        } else {
            icmpv4::TYPE_ECHO_REQUEST
        },
        0,
        0,
        0,
        0,
        0,
    ]);
    message.extend_from_slice(&sequence.to_be_bytes());
    message.extend_from_slice(payload);
    message
}

pub fn build_reply(
    remote: IpAddr,
    client: IpAddr,
    hop_limit: u8,
    identification: Option<u16>,
    identity: Identity,
    payload: &[u8],
) -> Result<Vec<u8>, WriterError> {
    build(
        false,
        remote,
        client,
        hop_limit,
        identification,
        identity,
        payload,
    )
}

pub fn build_request_packet(
    client: IpAddr,
    remote: IpAddr,
    hop_limit: u8,
    identity: Identity,
    payload: &[u8],
) -> Result<Vec<u8>, WriterError> {
    build(true, client, remote, hop_limit, None, identity, payload)
}

fn build(
    request: bool,
    source: IpAddr,
    destination: IpAddr,
    hop_limit: u8,
    identification: Option<u16>,
    identity: Identity,
    payload: &[u8],
) -> Result<Vec<u8>, WriterError> {
    let echo = IcmpEchoHeader {
        id: identity.identifier,
        seq: identity.sequence,
    };
    match (source, destination) {
        (IpAddr::V4(remote), IpAddr::V4(client)) => {
            let icmp = Icmpv4Header::with_checksum(
                if request {
                    Icmpv4Type::EchoRequest(echo)
                } else {
                    Icmpv4Type::EchoReply(echo)
                },
                payload,
            );
            let mut ip = Ipv4Header {
                time_to_live: hop_limit,
                protocol: IpNumber::ICMP,
                source: remote.octets(),
                destination: client.octets(),
                identification: identification.unwrap_or(0),
                dont_fragment: identification.is_none(),
                ..Default::default()
            };
            ip.set_payload_len(ECHO_HEADER_LEN + payload.len())
                .map_err(|_| WriterError::Malformed("echo reply exceeds an IPv4 datagram"))?;
            ip.header_checksum = ip.calc_header_checksum();
            Ok(assemble(&ip.to_bytes(), &icmp.to_bytes(), payload))
        }
        (IpAddr::V6(remote), IpAddr::V6(client)) => {
            let mut ip = Ipv6Header {
                next_header: IpNumber::IPV6_ICMP,
                hop_limit,
                source: remote.octets(),
                destination: client.octets(),
                ..Default::default()
            };
            ip.set_payload_length(ECHO_HEADER_LEN + payload.len())
                .map_err(|_| WriterError::Malformed("echo reply exceeds an IPv6 datagram"))?;
            let icmp = Icmpv6Header::with_checksum(
                if request {
                    Icmpv6Type::EchoRequest(echo)
                } else {
                    Icmpv6Type::EchoReply(echo)
                },
                ip.source,
                ip.destination,
                payload,
            )
            .map_err(|_| WriterError::Malformed("echo reply payload too long"))?;
            Ok(assemble(&ip.to_bytes(), &icmp.to_bytes(), payload))
        }
        _ => Err(WriterError::Malformed(
            "echo reply crosses address families",
        )),
    }
}

struct Echo<'a> {
    checksum: u16,
    identifier: u16,
    sequence: u16,
    payload: &'a [u8],
}

impl Echo<'_> {
    fn header(&self) -> IcmpEchoHeader {
        IcmpEchoHeader {
            id: self.identifier,
            seq: self.sequence,
        }
    }

    fn identity(&self) -> Identity {
        Identity {
            identifier: self.identifier,
            sequence: self.sequence,
        }
    }
}

fn message(slice: &[u8], expected: u8) -> Result<Echo<'_>, Reject> {
    if slice.len() < ECHO_HEADER_LEN {
        return Err(Reject::Malformed("ICMP echo header does not fit"));
    }
    if slice[0] != expected {
        return Err(Reject::NotUdp);
    }
    if slice[1] != 0 {
        return Err(Reject::Malformed("ICMP echo code is not zero"));
    }
    Ok(Echo {
        checksum: u16::from_be_bytes([slice[2], slice[3]]),
        identifier: u16::from_be_bytes([slice[4], slice[5]]),
        sequence: u16::from_be_bytes([slice[6], slice[7]]),
        payload: &slice[ECHO_HEADER_LEN..],
    })
}

fn assemble(ip: &[u8], icmp: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(ip.len() + icmp.len() + payload.len());
    packet.extend_from_slice(ip);
    packet.extend_from_slice(icmp);
    packet.extend_from_slice(payload);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::packet_writer::{validate, IPV4_HEADER_LEN, IPV6_HEADER_LEN};

    const ONE: Identity = Identity {
        identifier: 1,
        sequence: 1,
    };

    const CLIENT4: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    const REMOTE4: IpAddr = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));
    const CLIENT6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 2));
    const REMOTE6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111));

    fn client_request(
        client: IpAddr,
        remote: IpAddr,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let echo = IcmpEchoHeader {
            id: identifier,
            seq: sequence,
        };
        match (client, remote) {
            (IpAddr::V4(client), IpAddr::V4(remote)) => {
                let icmp = Icmpv4Header::with_checksum(Icmpv4Type::EchoRequest(echo), payload);
                let mut ip = Ipv4Header {
                    time_to_live: 64,
                    protocol: IpNumber::ICMP,
                    source: client.octets(),
                    destination: remote.octets(),
                    dont_fragment: true,
                    ..Default::default()
                };
                ip.set_payload_len(ECHO_HEADER_LEN + payload.len()).unwrap();
                ip.header_checksum = ip.calc_header_checksum();
                assemble(&ip.to_bytes(), &icmp.to_bytes(), payload)
            }
            (IpAddr::V6(client), IpAddr::V6(remote)) => {
                let mut ip = Ipv6Header {
                    next_header: IpNumber::IPV6_ICMP,
                    hop_limit: 64,
                    source: client.octets(),
                    destination: remote.octets(),
                    ..Default::default()
                };
                ip.set_payload_length(ECHO_HEADER_LEN + payload.len())
                    .unwrap();
                let icmp = Icmpv6Header::with_checksum(
                    Icmpv6Type::EchoRequest(echo),
                    ip.source,
                    ip.destination,
                    payload,
                )
                .unwrap();
                assemble(&ip.to_bytes(), &icmp.to_bytes(), payload)
            }
            _ => unreachable!("test addresses are same-family"),
        }
    }

    #[test]
    fn a_request_parses_into_what_the_table_keys_on() {
        for (client, remote) in [(CLIENT4, REMOTE4), (CLIENT6, REMOTE6)] {
            let packet = client_request(client, remote, 0xbeef, 7, b"probe");
            let request = parse(&packet).unwrap();
            assert_eq!(request.client, client);
            assert_eq!(request.remote, remote);
            assert_eq!(request.hop_limit, 64);
            assert!(request.dont_fragment);
            assert_eq!(
                request.identity,
                Identity {
                    identifier: 0xbeef,
                    sequence: 7
                }
            );
            assert_eq!(request.payload, b"probe");
        }
    }

    #[test]
    fn a_corrupted_checksum_is_rejected_in_both_families() {
        for (client, remote) in [(CLIENT4, REMOTE4), (CLIENT6, REMOTE6)] {
            let mut packet = client_request(client, remote, 1, 1, b"probe");
            *packet.last_mut().unwrap() ^= 0xff;
            assert_eq!(
                parse(&packet),
                Err(Reject::Malformed(if client.is_ipv6() {
                    "ICMPv6 echo checksum mismatch"
                } else {
                    "ICMPv4 echo checksum mismatch"
                }))
            );
        }
    }

    #[test]
    fn only_echo_requests_are_accepted() {
        for (client, remote) in [(CLIENT4, REMOTE4), (CLIENT6, REMOTE6)] {
            let packet = build_reply(client, remote, 64, None, ONE, b"probe").unwrap();
            assert_eq!(parse(&packet), Err(Reject::NotUdp));
        }
        let mut packet = client_request(CLIENT4, REMOTE4, 1, 1, b"probe");
        packet[IPV4_HEADER_LEN + 1] = 3;
        assert_eq!(
            parse(&packet),
            Err(Reject::Malformed("ICMP echo code is not zero"))
        );
    }

    #[test]
    fn a_fragmented_request_is_separated_from_an_unsupported_one() {
        let mut packet = client_request(CLIENT4, REMOTE4, 1, 1, b"probe");
        packet[6] = 0;
        packet[7] = 1;
        assert_eq!(parse(&packet), Err(Reject::Fragmented));
        let mut packet = client_request(CLIENT6, REMOTE6, 1, 1, b"probe");
        packet[6] = IpNumber::IPV6_FRAGMENTATION_HEADER.0;
        assert_eq!(parse(&packet), Err(Reject::Fragmented));
    }

    #[test]
    fn a_reply_restores_the_clients_identity_and_is_writable() {
        for (client, remote) in [(CLIENT4, REMOTE4), (CLIENT6, REMOTE6)] {
            let packet = build_reply(
                remote,
                client,
                42,
                None,
                Identity {
                    identifier: 0xbeef,
                    sequence: 7,
                },
                b"probe",
            )
            .unwrap();
            assert_eq!(validate(&packet, 1500), Ok(()), "{client}");
            let header = if client.is_ipv6() {
                IPV6_HEADER_LEN
            } else {
                IPV4_HEADER_LEN
            };
            assert_eq!(
                packet[header],
                if client.is_ipv6() {
                    icmpv6::TYPE_ECHO_REPLY
                } else {
                    icmpv4::TYPE_ECHO_REPLY
                }
            );
            let (reply, payload) = peek_reply(&packet[header..], client.is_ipv6()).unwrap();
            assert_eq!(reply.identifier, 0xbeef);
            assert_eq!(reply.sequence, 7);
            assert_eq!(payload, b"probe");
        }
    }

    #[test]
    fn a_reply_clears_df_exactly_when_it_carries_an_identification() {
        let atomic = build_reply(REMOTE4, CLIENT4, 42, None, ONE, b"probe").unwrap();
        assert_eq!(u16::from_be_bytes([atomic[6], atomic[7]]) & 0x4000, 0x4000);
        let fragmentable = build_reply(REMOTE4, CLIENT4, 42, Some(0x1234), ONE, b"probe").unwrap();
        assert_eq!(
            u16::from_be_bytes([fragmentable[6], fragmentable[7]]) & 0x4000,
            0
        );
        assert_eq!(
            u16::from_be_bytes([fragmentable[4], fragmentable[5]]),
            0x1234
        );
    }

    #[test]
    fn a_request_to_a_ping_socket_leaves_the_kernels_fields_alone() {
        for ipv6 in [false, true] {
            let message = build_request(ipv6, 0x2211, b"probe");
            assert_eq!(
                message[0],
                if ipv6 {
                    icmpv6::TYPE_ECHO_REQUEST
                } else {
                    icmpv4::TYPE_ECHO_REQUEST
                }
            );
            assert_eq!(&message[1..6], &[0, 0, 0, 0, 0]);
            assert_eq!(u16::from_be_bytes([message[6], message[7]]), 0x2211);
            assert_eq!(&message[ECHO_HEADER_LEN..], b"probe");
        }
    }

    #[test]
    fn crossing_families_is_refused() {
        assert!(build_reply(REMOTE4, CLIENT6, 42, None, ONE, b"probe").is_err());
        assert!(build_reply(REMOTE6, CLIENT4, 42, None, ONE, b"probe").is_err());
        assert_eq!(
            parse(&[]),
            Err(Reject::Malformed("not an IPv4 or IPv6 packet"))
        );
    }
}
