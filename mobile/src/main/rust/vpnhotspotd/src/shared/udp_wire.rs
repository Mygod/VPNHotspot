//! Wire format for relayed UDP: the strict parse of a client datagram read from the TUN, and the
//! construction of the reply that goes back out through the common writer.
//!
//! Platform-neutral and unit tested, because every byte here is chosen by whoever put the packet on the
//! interface - which, per the security posture, is any local app and not only a tethered client.
//!
//! The parse is strict rather than forgiving on purpose. A relay that guesses at a self-inconsistent
//! datagram re-originates it from a different source address, and the guess then arrives at the remote
//! looking authoritative. Every field the daemon repeats is therefore one it verified, and everything
//! else is a rejection with a reason the caller can count.

#[cfg(test)]
use std::net::{Ipv4Addr, Ipv6Addr};
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};

use etherparse::{IpFragOffset, IpNumber, Ipv4Header, Ipv6Header, UdpHeader};

use crate::shared::ip_wire::{ipv6_payload, Ipv6Payload, Packet};
use crate::shared::packet_writer::WriterError;

/// One client datagram, in the terms the relay keys and forwards on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Relayed<'a> {
    /// The TUN-visible source, which is the mapping key. Never an identity: nothing distinguishes a
    /// tethered client's address from one a local app chose.
    pub source: SocketAddr,
    pub destination: SocketAddr,
    /// As it arrived, not yet decremented. The forwarding decision belongs to the caller, which is also
    /// the only thing that can answer an expired one.
    pub hop_limit: u8,
    /// IPv4 DF, which the mapping owner reapplies to the shared upstream socket before each send.
    ///
    /// True for IPv6, where it is not a bit at all: no router may fragment an IPv6 packet, so the
    /// permission the IPv4 bit withholds is withheld unconditionally there.
    pub dont_fragment: bool,
    pub payload: &'a [u8],
}

/// Why a datagram is not relayed. Each variant is counted rather than logged, since the caller sees
/// attacker-influenced input and one report per packet would be a flood by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// Some other transport. TCP and ICMP have their own paths; anything else has none.
    NotUdp,
    /// A fragment. Reassembly is its own bounded context, so this parse never sees a whole datagram it
    /// could relay.
    Fragmented,
    /// An IPv6 extension-header chain. The bounded walk over one belongs with reassembly too.
    Extended,
    /// Truncated, self-inconsistent, or carrying a checksum that does not match what it covers.
    Malformed(&'static str),
}

pub fn parse(packet: &[u8]) -> Result<Relayed<'_>, Reject> {
    match Packet::parse(packet).map_err(|error| Reject::Malformed(error.message()))? {
        Packet::Ipv4 { header, payload } => {
            if header.protocol() != IpNumber::UDP {
                return Err(Reject::NotUdp);
            }
            // more-fragments, or a non-zero offset
            if header.more_fragments() || header.fragments_offset() != IpFragOffset::ZERO {
                return Err(Reject::Fragmented);
            }
            let source = header.source_addr();
            let destination = header.destination_addr();
            let (udp, payload) = transport(payload)?;
            // Zero means the sender declined to compute one, which IPv4 permits. Any other value is
            // verified, because the datagram leaves on a different source address and so acquires a
            // fresh valid checksum: this is the only place corruption can still be seen.
            if udp.checksum != 0
                && udp
                    .calc_checksum_ipv4_raw(source.octets(), destination.octets(), payload)
                    .map_err(|_| Reject::Malformed("IPv4 UDP payload too long"))?
                    != udp.checksum
            {
                return Err(Reject::Malformed("IPv4 UDP checksum mismatch"));
            }
            Ok(Relayed {
                source: SocketAddr::V4(SocketAddrV4::new(source, udp.source_port)),
                destination: SocketAddr::V4(SocketAddrV4::new(destination, udp.destination_port)),
                hop_limit: header.ttl(),
                dont_fragment: header.dont_fragment(),
                payload,
            })
        }
        Packet::Ipv6 { header, payload } => {
            // the extension-header set comes from the library rather than a list repeated here, so a
            // chain this parse cannot walk is never mistaken for an unsupported transport
            match ipv6_payload(header.next_header(), IpNumber::UDP) {
                Ipv6Payload::Transport => {}
                Ipv6Payload::Fragment => return Err(Reject::Fragmented),
                Ipv6Payload::Extension => return Err(Reject::Extended),
                Ipv6Payload::Other => return Err(Reject::NotUdp),
            }
            let source = header.source_addr();
            let destination = header.destination_addr();
            let (udp, payload) = transport(payload)?;
            // Mandatory over IPv6, so zero is not "omitted" but wrong, and 0xffff is what a true zero
            // is transmitted as.
            if udp.checksum == 0 {
                return Err(Reject::Malformed("IPv6 UDP checksum absent"));
            }
            if udp
                .calc_checksum_ipv6_raw(source.octets(), destination.octets(), payload)
                .map_err(|_| Reject::Malformed("IPv6 UDP payload too long"))?
                != udp.checksum
            {
                return Err(Reject::Malformed("IPv6 UDP checksum mismatch"));
            }
            Ok(Relayed {
                source: SocketAddr::V6(SocketAddrV6::new(source, udp.source_port, 0, 0)),
                destination: SocketAddr::V6(SocketAddrV6::new(
                    destination,
                    udp.destination_port,
                    0,
                    0,
                )),
                hop_limit: header.hop_limit(),
                dont_fragment: true,
                payload,
            })
        }
    }
}

/// The UDP header and exactly the bytes it declares. Trailing bytes are a rejection rather than padding:
/// the IP layer already said how long the datagram is, so a disagreement means one of the two is lying
/// and there is no way to tell which.
fn transport(slice: &[u8]) -> Result<(UdpHeader, &[u8]), Reject> {
    let (header, rest) =
        UdpHeader::from_slice(slice).map_err(|_| Reject::Malformed("UDP header does not fit"))?;
    if header.length as usize != slice.len() {
        return Err(Reject::Malformed("UDP length disagrees"));
    }
    // port 0 is reserved and never a real endpoint, so a mapping keyed on it could never be replied to
    if header.source_port == 0 || header.destination_port == 0 {
        return Err(Reject::Malformed("UDP port 0"));
    }
    Ok((header, rest))
}

/// Builds the TUN-side packet for one reply, from the remote that sent it to the client that asked.
///
/// `identification` decides the IPv4 fragmentation permission and is the whole DF policy in one
/// argument: `None` sets DF, which is what every packet within the downstream floor gets, and `Some`
/// clears it and carries the guarded value that Android's downstream fragmentation will then repeat into
/// every fragment. Passing `Some` for a packet that fits would hand out an Identification for nothing;
/// passing `None` for one that does not would make Android drop it and answer with Fragmentation Needed.
///
/// IPv6 ignores it: the reply is either within the limit or source-fragmented by the caller, and its
/// Identification is 32 bits wide rather than 16.
pub fn build_reply(
    remote: SocketAddr,
    client: SocketAddr,
    hop_limit: u8,
    identification: Option<u16>,
    payload: &[u8],
) -> Result<Vec<u8>, WriterError> {
    match (remote, client) {
        (SocketAddr::V4(remote), SocketAddr::V4(client)) => {
            let mut ip = Ipv4Header {
                time_to_live: hop_limit,
                protocol: IpNumber::UDP,
                source: remote.ip().octets(),
                destination: client.ip().octets(),
                identification: identification.unwrap_or(0),
                dont_fragment: identification.is_none(),
                ..Default::default()
            };
            ip.set_payload_len(UdpHeader::LEN + payload.len())
                .map_err(|_| WriterError::Malformed("reply exceeds an IPv4 datagram"))?;
            let udp = UdpHeader::with_ipv4_checksum(remote.port(), client.port(), &ip, payload)
                .map_err(|_| WriterError::Malformed("reply exceeds a UDP datagram"))?;
            // last, because to_bytes serializes the stored value rather than recomputing it
            ip.header_checksum = ip.calc_header_checksum();
            Ok(assemble(&ip.to_bytes(), &udp, payload))
        }
        (SocketAddr::V6(remote), SocketAddr::V6(client)) => {
            let mut ip = Ipv6Header {
                next_header: IpNumber::UDP,
                hop_limit,
                source: remote.ip().octets(),
                destination: client.ip().octets(),
                ..Default::default()
            };
            ip.set_payload_length(UdpHeader::LEN + payload.len())
                .map_err(|_| WriterError::Malformed("reply exceeds an IPv6 datagram"))?;
            let udp = UdpHeader::with_ipv6_checksum(remote.port(), client.port(), &ip, payload)
                .map_err(|_| WriterError::Malformed("reply exceeds a UDP datagram"))?;
            Ok(assemble(&ip.to_bytes(), &udp, payload))
        }
        _ => Err(WriterError::Malformed("reply crosses address families")),
    }
}

fn assemble(ip: &[u8], udp: &UdpHeader, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(ip.len() + UdpHeader::LEN + payload.len());
    packet.extend_from_slice(ip);
    packet.extend_from_slice(&udp.to_bytes());
    packet.extend_from_slice(payload);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::packet_writer::{validate, IPV4_HEADER_LEN, IPV6_HEADER_LEN};

    const CLIENT4: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)), 4242);
    const REMOTE4: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1)), 53);
    const CLIENT6: SocketAddr = SocketAddr::new(
        std::net::IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 2)),
        4242,
    );
    const REMOTE6: SocketAddr = SocketAddr::new(
        std::net::IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
        53,
    );

    /// A client datagram as it would arrive on the TUN, built through the same builder the reply path
    /// uses so that the test exercises the parse rather than a hand-rolled encoder.
    fn client_datagram(client: SocketAddr, remote: SocketAddr, payload: &[u8]) -> Vec<u8> {
        build_reply(client, remote, 64, None, payload).unwrap()
    }

    #[test]
    fn a_well_formed_datagram_round_trips() {
        for (client, remote) in [(CLIENT4, REMOTE4), (CLIENT6, REMOTE6)] {
            let packet = client_datagram(client, remote, b"query");
            assert_eq!(
                parse(&packet),
                Ok(Relayed {
                    source: client,
                    destination: remote,
                    hop_limit: 64,
                    dont_fragment: true,
                    payload: b"query",
                })
            );
        }
    }

    #[test]
    fn a_cleared_ipv4_df_bit_is_reported() {
        let packet = build_reply(CLIENT4, REMOTE4, 64, Some(7), b"query").unwrap();
        assert!(!parse(&packet).unwrap().dont_fragment);
        assert_eq!(u16::from_be_bytes([packet[4], packet[5]]), 7);
    }

    #[test]
    fn corruption_is_caught_by_the_checksum() {
        for (client, remote) in [(CLIENT4, REMOTE4), (CLIENT6, REMOTE6)] {
            let mut packet = client_datagram(client, remote, b"query");
            *packet.last_mut().unwrap() ^= 0xff;
            assert!(matches!(parse(&packet), Err(Reject::Malformed(_))));
        }
    }

    #[test]
    fn an_omitted_ipv4_checksum_is_accepted_but_a_missing_ipv6_one_is_not() {
        let mut packet = client_datagram(CLIENT4, REMOTE4, b"query");
        let checksum = IPV4_HEADER_LEN + 6;
        packet[checksum..checksum + 2].copy_from_slice(&[0, 0]);
        assert_eq!(parse(&packet).unwrap().payload, b"query");

        let mut packet = client_datagram(CLIENT6, REMOTE6, b"query");
        let checksum = IPV6_HEADER_LEN + 6;
        packet[checksum..checksum + 2].copy_from_slice(&[0, 0]);
        assert_eq!(
            parse(&packet),
            Err(Reject::Malformed("IPv6 UDP checksum absent"))
        );
    }

    #[test]
    fn fragments_and_other_transports_are_separated_from_malformed_input() {
        let mut packet = client_datagram(CLIENT4, REMOTE4, b"query");
        // more-fragments, which makes this a first fragment rather than a whole datagram
        packet[6] |= 0x20;
        assert_eq!(parse(&packet), Err(Reject::Fragmented));

        let mut packet = client_datagram(CLIENT4, REMOTE4, b"query");
        packet[9] = IpNumber::TCP.0;
        assert_eq!(parse(&packet), Err(Reject::NotUdp));

        let mut packet = client_datagram(CLIENT6, REMOTE6, b"query");
        packet[6] = IpNumber::IPV6_FRAGMENTATION_HEADER.0;
        assert_eq!(parse(&packet), Err(Reject::Fragmented));

        let mut packet = client_datagram(CLIENT6, REMOTE6, b"query");
        packet[6] = IpNumber::IPV6_HEADER_HOP_BY_HOP.0;
        assert_eq!(parse(&packet), Err(Reject::Extended));

        let mut packet = client_datagram(CLIENT6, REMOTE6, b"query");
        packet[6] = IpNumber::IPV6_ICMP.0;
        assert_eq!(parse(&packet), Err(Reject::NotUdp));
    }

    #[test]
    fn self_inconsistent_lengths_are_rejected() {
        for (client, remote) in [(CLIENT4, REMOTE4), (CLIENT6, REMOTE6)] {
            let mut packet = client_datagram(client, remote, b"query");
            packet.pop();
            assert!(matches!(parse(&packet), Err(Reject::Malformed(_))));
        }
        // a UDP length shorter than the IP layer says, which would leave trailing bytes to guess about
        let mut packet = client_datagram(CLIENT4, REMOTE4, b"query");
        let length = IPV4_HEADER_LEN + 4;
        packet[length..length + 2].copy_from_slice(&(UdpHeader::LEN as u16).to_be_bytes());
        assert_eq!(
            parse(&packet),
            Err(Reject::Malformed("UDP length disagrees"))
        );
        assert!(matches!(parse(&[]), Err(Reject::Malformed(_))));
        assert!(matches!(parse(&[0x45; 8]), Err(Reject::Malformed(_))));
    }

    #[test]
    fn port_zero_is_rejected() {
        let packet = client_datagram(
            SocketAddr::new(CLIENT4.ip(), 0),
            SocketAddr::new(REMOTE4.ip(), 53),
            b"query",
        );
        assert_eq!(parse(&packet), Err(Reject::Malformed("UDP port 0")));
    }

    #[test]
    fn replies_are_writable_packets_addressed_back_to_the_client() {
        for (client, remote) in [(CLIENT4, REMOTE4), (CLIENT6, REMOTE6)] {
            let reply = build_reply(remote, client, 57, None, b"answer").unwrap();
            assert_eq!(validate(&reply, 1500), Ok(()));
            let parsed = parse(&reply).unwrap();
            assert_eq!(parsed.source, remote);
            assert_eq!(parsed.destination, client);
            assert_eq!(parsed.hop_limit, 57);
            assert_eq!(parsed.payload, b"answer");
        }
    }

    #[test]
    fn a_reply_across_families_is_refused() {
        assert_eq!(
            build_reply(REMOTE4, CLIENT6, 64, None, b"answer"),
            Err(WriterError::Malformed("reply crosses address families"))
        );
    }
}
