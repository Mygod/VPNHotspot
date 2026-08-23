//! Just enough of a TCP segment to route it: which flow it belongs to, and whether it opens one.
//!
//! Deliberately not a parse of the whole header. The terminating stack owns sequence numbers, windows, options
//! and state; what the engine needs before handing a packet over is the four-tuple to key on, the hop limit to
//! validate, and whether this is a SYN - because a SYN for an unknown destination is the one packet that has to
//! create a listening socket *before* the stack sees it.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use etherparse::IpNumber;

use crate::shared::packet_writer::{IPV4_HEADER_LEN, IPV6_HEADER_LEN};
use crate::shared::udp_wire::Reject;

/// The minimum TCP header, which is as far as the flags and ports reach.
const TCP_HEADER_LEN: usize = 20;
const FLAG_SYN: u8 = 0x02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// TUN-visible source. For IPv4 this is Android's own inner NAT address rather than the client's, which is
    /// why it is never treated as an identity.
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub hop_limit: u8,
    pub syn: bool,
}

pub fn peek(packet: &[u8]) -> Result<Segment, Reject> {
    let (header, hop_limit, source, destination) = match packet.first().map(|byte| byte >> 4) {
        Some(4) => {
            let header = ((packet[0] & 0xf) as usize) * 4;
            if header < IPV4_HEADER_LEN || packet.len() < header {
                return Err(Reject::Malformed("IPv4 header does not fit"));
            }
            if packet[9] != IpNumber::TCP.0 {
                return Err(Reject::NotUdp);
            }
            let flags = u16::from_be_bytes([packet[6], packet[7]]);
            if flags & 0x2000 != 0 || flags & 0x1fff != 0 {
                return Err(Reject::Fragmented);
            }
            (
                header,
                packet[8],
                std::net::IpAddr::V4(Ipv4Addr::from(
                    <[u8; 4]>::try_from(&packet[12..16]).unwrap(),
                )),
                std::net::IpAddr::V4(Ipv4Addr::from(
                    <[u8; 4]>::try_from(&packet[16..20]).unwrap(),
                )),
            )
        }
        Some(6) => {
            if packet.len() < IPV6_HEADER_LEN {
                return Err(Reject::Malformed("IPv6 header does not fit"));
            }
            match IpNumber(packet[6]) {
                IpNumber::TCP => {}
                IpNumber::IPV6_FRAGMENTATION_HEADER => return Err(Reject::Fragmented),
                header if header.is_ipv6_ext_header_value() => return Err(Reject::Extended),
                _ => return Err(Reject::NotUdp),
            }
            (
                IPV6_HEADER_LEN,
                packet[7],
                std::net::IpAddr::V6(Ipv6Addr::from(
                    <[u8; 16]>::try_from(&packet[8..24]).unwrap(),
                )),
                std::net::IpAddr::V6(Ipv6Addr::from(
                    <[u8; 16]>::try_from(&packet[24..40]).unwrap(),
                )),
            )
        }
        _ => return Err(Reject::Malformed("not an IPv4 or IPv6 packet")),
    };
    let transport = packet
        .get(header..header + TCP_HEADER_LEN)
        .ok_or(Reject::Malformed("TCP header does not fit"))?;
    let source_port = u16::from_be_bytes([transport[0], transport[1]]);
    let destination_port = u16::from_be_bytes([transport[2], transport[3]]);
    if source_port == 0 || destination_port == 0 {
        return Err(Reject::Malformed("TCP port 0"));
    }
    Ok(Segment {
        source: endpoint(source, source_port),
        destination: endpoint(destination, destination_port),
        hop_limit,
        // Bit 1 of the flags byte. SYN-ACK is not a client packet on this interface, so no distinction is
        // needed here; the stack rejects one for a flow it has no state for.
        syn: transport[13] & FLAG_SYN != 0,
    })
}

fn endpoint(address: std::net::IpAddr, port: u16) -> SocketAddr {
    match address {
        std::net::IpAddr::V4(address) => SocketAddr::V4(SocketAddrV4::new(address, port)),
        std::net::IpAddr::V6(address) => SocketAddr::V6(SocketAddrV6::new(address, port, 0, 0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An IPv4 TCP segment with the flags byte the caller asks for.
    fn ipv4(flags: u8) -> Vec<u8> {
        let mut packet = vec![0x45, 0, 0, 40, 0, 0, 0, 0, 64, IpNumber::TCP.0, 0, 0];
        packet.extend_from_slice(&[192, 0, 2, 1]);
        packet.extend_from_slice(&[198, 51, 100, 7]);
        packet.extend_from_slice(&40000u16.to_be_bytes());
        packet.extend_from_slice(&443u16.to_be_bytes());
        packet.extend_from_slice(&[0; 8]);
        packet.push(0x50);
        packet.push(flags);
        packet.extend_from_slice(&[0; 6]);
        packet
    }

    #[test]
    fn a_syn_names_its_flow_and_announces_itself() {
        let segment = peek(&ipv4(FLAG_SYN)).unwrap();
        assert_eq!(segment.source, "192.0.2.1:40000".parse().unwrap());
        assert_eq!(segment.destination, "198.51.100.7:443".parse().unwrap());
        assert_eq!(segment.hop_limit, 64);
        assert!(segment.syn);
        // an ACK carrying data is the same flow and not a new one
        assert!(!peek(&ipv4(0x10)).unwrap().syn);
    }

    #[test]
    fn other_protocols_fragments_and_truncation_are_separated() {
        let mut packet = ipv4(FLAG_SYN);
        packet[9] = IpNumber::UDP.0;
        assert_eq!(peek(&packet), Err(Reject::NotUdp));

        let mut packet = ipv4(FLAG_SYN);
        packet[6] |= 0x20;
        assert_eq!(peek(&packet), Err(Reject::Fragmented));

        let packet = ipv4(FLAG_SYN);
        assert!(matches!(peek(&packet[..30]), Err(Reject::Malformed(_))));
        assert!(matches!(peek(&[]), Err(Reject::Malformed(_))));
    }
}
