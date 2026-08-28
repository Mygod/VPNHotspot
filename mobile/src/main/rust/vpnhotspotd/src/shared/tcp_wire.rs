//! Just enough of a TCP segment to route it: which flow it belongs to, and whether it opens one.
//!
//! Deliberately not a parse of the whole header. The terminating stack owns sequence numbers, windows, options
//! and state; what the engine needs before handing a packet over is the four-tuple to key on, the hop limit to
//! validate, and whether this is a SYN - because a SYN for an unknown destination is the one packet that has to
//! create a listening socket *before* the stack sees it.

use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};

use etherparse::{IpFragOffset, IpNumber};

use crate::shared::ip_wire::{ipv6_payload, Ipv6Payload, Packet};
use crate::shared::udp_wire::Reject;

/// The minimum TCP header, which is as far as the flags and ports reach.
const TCP_HEADER_LEN: usize = 20;
const FLAG_SYN: u8 = 0x02;
const FLAG_RST: u8 = 0x04;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// TUN-visible source. For IPv4 this is Android's own inner NAT address rather than the client's, which is
    /// why it is never treated as an identity.
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub hop_limit: u8,
    pub syn: bool,
    /// This segment *claims* to abort the connection.
    ///
    /// A candidate and nothing more. Nothing here validates a checksum, a sequence number or a window, and
    /// the stack refuses a reset outright in `LISTEN`, so a segment carrying this bit is not yet a reset -
    /// and acting on it as though it were poisoned flows named by packets `smoltcp` then threw away.
    ///
    /// The *cause* is the transition the stack makes when it accepts one, observed across the poll that
    /// processed this exact packet - see `shizuku::tcp::ingress`. This flag is carried only so that owner
    /// knows which packets are worth watching a socket across.
    pub rst: bool,
}

pub fn peek(packet: &[u8]) -> Result<Segment, Reject> {
    let (transport, hop_limit, source, destination) =
        match Packet::parse(packet).map_err(|error| Reject::Malformed(error.message()))? {
            Packet::Ipv4 { header, payload } => {
                if header.protocol() != IpNumber::TCP {
                    return Err(Reject::NotUdp);
                }
                if header.more_fragments() || header.fragments_offset() != IpFragOffset::ZERO {
                    return Err(Reject::Fragmented);
                }
                (
                    payload,
                    header.ttl(),
                    std::net::IpAddr::V4(header.source_addr()),
                    std::net::IpAddr::V4(header.destination_addr()),
                )
            }
            Packet::Ipv6 { header, payload } => {
                match ipv6_payload(header.next_header(), IpNumber::TCP) {
                    Ipv6Payload::Transport => {}
                    Ipv6Payload::Fragment => return Err(Reject::Fragmented),
                    Ipv6Payload::Extension => return Err(Reject::Extended),
                    Ipv6Payload::Other => return Err(Reject::NotUdp),
                }
                (
                    payload,
                    header.hop_limit(),
                    std::net::IpAddr::V6(header.source_addr()),
                    std::net::IpAddr::V6(header.destination_addr()),
                )
            }
        };
    let transport = transport
        .get(..TCP_HEADER_LEN)
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
        // Bit 2, and no more than the bit: what a reset *is* is what the stack accepts, which this parse is
        // in no position to judge.
        rst: transport[13] & FLAG_RST != 0,
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
        assert!(!segment.rst);
        // an ACK carrying data is the same flow and not a new one
        assert!(!peek(&ipv4(0x10)).unwrap().syn);
    }

    #[test]
    fn a_reset_bit_is_read_as_a_candidate_and_nothing_more() {
        // What the flag is for: telling the owner which packets to watch a socket across. Whether any of
        // these is a reset at all is the stack's answer, not this one's.
        let segment = peek(&ipv4(FLAG_RST)).unwrap();
        assert!(segment.rst);
        assert!(!segment.syn, "a reset is not an opening");
        assert_eq!(segment.source, "192.0.2.1:40000".parse().unwrap());
        // A reset acknowledging the peer's own FIN is the case that matters - it names a flow whose clean
        // ending this daemon may already have extracted.
        let after_fin = peek(&ipv4(FLAG_RST | 0x10)).unwrap();
        assert!(after_fin.rst);
        // And an ordinary segment carries neither.
        let plain = peek(&ipv4(0x10)).unwrap();
        assert!(!plain.rst);
        assert!(!plain.syn);
        // FIN alone is an ending, never an abort.
        assert!(!peek(&ipv4(0x11)).unwrap().rst);
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
