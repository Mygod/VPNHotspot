//! ICMP errors this daemon originates toward a client.
//!
//! Originates, not translates. These say something about the daemon's own forwarding decision - a hop limit
//! that ran out here, a datagram the upstream path will not carry - so the source address is the interface's
//! own, exactly as a router's would be, and the hop limit is a local origin value rather than anything
//! preserved. Translating an error the *remote* sent is a different problem with a different correctness
//! argument, and is not this module.
//!
//! The quote is truncated and never fragmented. An error large enough to need fragmenting would be one the
//! client might not reassemble, which defeats the point of sending it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use etherparse::{
    icmpv4, icmpv6, Icmpv4Header, Icmpv4Type, Icmpv6Header, Icmpv6Type, IpNumber, Ipv4Header,
    Ipv6Header,
};

use crate::shared::packet_writer::{WriterError, IPV4_HEADER_LEN, IPV6_HEADER_LEN};

/// Newly originated packets use an immutable local origin value, per MTU, Output, And Fragments.
const LOCAL_ORIGIN_HOP_LIMIT: u8 = 64;

/// RFC 1812 section 4.3.2.3: an ICMPv4 error should quote as much of the invoking datagram as possible without
/// the error itself exceeding 576 bytes, which every IPv4 host must be able to reassemble.
const ICMPV4_MAX_PACKET: usize = 576;

/// RFC 4443 section 2.4 (c): an ICMPv6 error must not exceed the IPv6 minimum MTU, so that it needs no
/// fragmentation to reach any host.
const ICMPV6_MAX_PACKET: usize = 1280;

/// What the daemon has to tell a client about a packet it could not forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The remaining hop limit reached zero here. A router owes this, and it is also what makes the daemon
    /// visible to a traceroute rather than a silent hole.
    Expired,
    /// The upstream path will not carry it and the client forbade fragmenting. The MTU is the one the kernel
    /// reported for that path, never a guess: a wrong value is cached by the client for minutes, so too small
    /// costs throughput for that whole window and too large keeps the black hole open while looking fixed.
    TooBig { mtu: u32 },
    /// A remote said the destination could not be reached, and the code is repeated exactly as it arrived.
    ///
    /// Never translated across families: the code spaces differ, so 3 means "port unreachable" over IPv4 and
    /// "address unreachable" over IPv6. This is only ever built for the family it arrived on.
    Unreachable { code: u8 },
    /// Fragments of one datagram were held until the reassembly timer ran out. Owed only when fragment zero
    /// arrived, because the error has to quote a header the client actually sent, and never for a context the
    /// daemon dropped under its own resource pressure - that says nothing about the path.
    ReassemblyExpired,
}

/// Builds one ICMP error about `invoking`, addressed from `source` back to whoever sent it.
///
/// `invoking` is the packet as it arrived, so its own header is what gets quoted - which is what lets the
/// client match the error to the socket that caused it.
pub fn build(source: IpAddr, invoking: &[u8], reason: Reason) -> Result<Vec<u8>, WriterError> {
    match (source, invoking.first().map(|byte| byte >> 4)) {
        (IpAddr::V4(source), Some(4)) => {
            if invoking.len() < IPV4_HEADER_LEN {
                return Err(WriterError::Malformed("invoking IPv4 packet has no header"));
            }
            let destination = Ipv4Addr::from(<[u8; 4]>::try_from(&invoking[12..16]).unwrap());
            let icmp = match reason {
                Reason::Expired => {
                    Icmpv4Type::TimeExceeded(icmpv4::TimeExceededCode::TtlExceededInTransit)
                }
                Reason::ReassemblyExpired => Icmpv4Type::TimeExceeded(
                    icmpv4::TimeExceededCode::FragmentReassemblyTimeExceeded,
                ),
                // Built as an unknown type/code pair rather than through the typed enum, because the point is
                // to repeat what the router said rather than to re-derive it from a meaning.
                Reason::Unreachable { code } => Icmpv4Type::Unknown {
                    type_u8: icmpv4::TYPE_DEST_UNREACH,
                    code_u8: code,
                    bytes5to8: [0; 4],
                },
                Reason::TooBig { mtu } => Icmpv4Type::DestinationUnreachable(
                    icmpv4::DestUnreachableHeader::FragmentationNeeded {
                        // 16 bits on the wire, and a path MTU beyond that cannot exist
                        next_hop_mtu: u16::try_from(mtu)
                            .map_err(|_| WriterError::Malformed("implausible path MTU"))?,
                    },
                ),
            };
            let quote = truncate(
                invoking,
                ICMPV4_MAX_PACKET - IPV4_HEADER_LEN - icmp.header_len(),
            );
            let icmp = Icmpv4Header::with_checksum(icmp, quote);
            let mut ip = Ipv4Header {
                time_to_live: LOCAL_ORIGIN_HOP_LIMIT,
                protocol: IpNumber::ICMP,
                source: source.octets(),
                destination: destination.octets(),
                ..Default::default()
            };
            ip.set_payload_len(icmp.header_len() + quote.len())
                .map_err(|_| WriterError::Malformed("ICMPv4 error does not fit a datagram"))?;
            ip.header_checksum = ip.calc_header_checksum();
            Ok(assemble(&ip.to_bytes(), &icmp.to_bytes(), quote))
        }
        (IpAddr::V6(source), Some(6)) => {
            if invoking.len() < IPV6_HEADER_LEN {
                return Err(WriterError::Malformed("invoking IPv6 packet has no header"));
            }
            let destination = Ipv6Addr::from(<[u8; 16]>::try_from(&invoking[8..24]).unwrap());
            let icmp = match reason {
                Reason::Expired => {
                    Icmpv6Type::TimeExceeded(icmpv6::TimeExceededCode::HopLimitExceeded)
                }
                Reason::ReassemblyExpired => Icmpv6Type::TimeExceeded(
                    icmpv6::TimeExceededCode::FragmentReassemblyTimeExceeded,
                ),
                Reason::Unreachable { code } => Icmpv6Type::Unknown {
                    type_u8: icmpv6::TYPE_DST_UNREACH,
                    code_u8: code,
                    bytes5to8: [0; 4],
                },
                Reason::TooBig { mtu } => Icmpv6Type::PacketTooBig { mtu },
            };
            let quote = truncate(
                invoking,
                ICMPV6_MAX_PACKET - IPV6_HEADER_LEN - icmp.header_len(),
            );
            let mut ip = Ipv6Header {
                next_header: IpNumber::IPV6_ICMP,
                hop_limit: LOCAL_ORIGIN_HOP_LIMIT,
                source: source.octets(),
                destination: destination.octets(),
                ..Default::default()
            };
            ip.set_payload_length(icmp.header_len() + quote.len())
                .map_err(|_| WriterError::Malformed("ICMPv6 error does not fit a datagram"))?;
            // ICMPv6's checksum covers a pseudo-header, unlike ICMPv4's, so it needs the addresses
            let icmp = Icmpv6Header::with_checksum(icmp, ip.source, ip.destination, quote)
                .map_err(|_| WriterError::Malformed("ICMPv6 error payload too long"))?;
            Ok(assemble(&ip.to_bytes(), &icmp.to_bytes(), quote))
        }
        // An error about a packet of one family cannot be sent from an address of another: the quote would
        // describe a header the client's stack would not parse.
        _ => Err(WriterError::Malformed(
            "ICMP error crosses address families",
        )),
    }
}

fn truncate(invoking: &[u8], room: usize) -> &[u8] {
    &invoking[..invoking.len().min(room)]
}

fn assemble(ip: &[u8], icmp: &[u8], quote: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(ip.len() + icmp.len() + quote.len());
    packet.extend_from_slice(ip);
    packet.extend_from_slice(icmp);
    packet.extend_from_slice(quote);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::packet_writer::validate;
    use crate::shared::udp_wire::build_reply;
    use std::net::SocketAddr;

    const GATEWAY4: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    const GATEWAY6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 1));

    /// A client datagram of `payload` bytes, which is what an error would be about.
    fn invoking(ipv6: bool, payload: usize) -> Vec<u8> {
        let (client, remote): (SocketAddr, SocketAddr) = if ipv6 {
            (
                "[2001:db8:1::2]:40000".parse().unwrap(),
                "[2606:4700::1111]:443".parse().unwrap(),
            )
        } else {
            (
                "192.0.2.1:40000".parse().unwrap(),
                "198.51.100.7:443".parse().unwrap(),
            )
        };
        build_reply(client, remote, 1, None, &vec![0x5au8; payload]).unwrap()
    }

    #[test]
    fn errors_are_writable_and_addressed_back_to_the_sender() {
        for (gateway, ipv6) in [(GATEWAY4, false), (GATEWAY6, true)] {
            for reason in [
                Reason::Expired,
                Reason::TooBig { mtu: 1400 },
                Reason::ReassemblyExpired,
                Reason::Unreachable { code: 3 },
            ] {
                let packet = build(gateway, &invoking(ipv6, 64), reason).unwrap();
                assert_eq!(validate(&packet, 1500), Ok(()), "{gateway} {reason:?}");
                let (source, destination) = if ipv6 {
                    (&packet[8..24], &packet[24..40])
                } else {
                    (&packet[12..16], &packet[16..20])
                };
                // from the interface, to whoever sent the invoking packet
                assert_eq!(source, gateway_octets(gateway).as_slice());
                assert_eq!(destination, client_octets(ipv6).as_slice());
            }
        }
    }

    fn gateway_octets(gateway: IpAddr) -> Vec<u8> {
        match gateway {
            IpAddr::V4(address) => address.octets().to_vec(),
            IpAddr::V6(address) => address.octets().to_vec(),
        }
    }

    fn client_octets(ipv6: bool) -> Vec<u8> {
        if ipv6 {
            Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 2)
                .octets()
                .to_vec()
        } else {
            Ipv4Addr::new(192, 0, 2, 1).octets().to_vec()
        }
    }

    #[test]
    fn a_huge_invoking_packet_is_truncated_rather_than_fragmented() {
        // 576 and 1280 are the sizes every host of each family must reassemble, so an error that needs
        // fragmenting to arrive would defeat its own purpose
        let packet = build(GATEWAY4, &invoking(false, 4000), Reason::Expired).unwrap();
        assert!(packet.len() <= ICMPV4_MAX_PACKET, "{}", packet.len());
        let invoking = invoking(true, 4000);
        let packet = build(GATEWAY6, &invoking, Reason::Expired).unwrap();
        assert!(packet.len() <= ICMPV6_MAX_PACKET, "{}", packet.len());
        // and the quote still starts with the invoking header, which is what lets the client match the error
        // to the socket that caused it
        let quote = IPV6_HEADER_LEN + 8;
        assert_eq!(
            &packet[quote..quote + IPV6_HEADER_LEN],
            &invoking[..IPV6_HEADER_LEN]
        );
    }

    #[test]
    fn the_reported_mtu_reaches_the_wire() {
        let packet = build(GATEWAY4, &invoking(false, 64), Reason::TooBig { mtu: 1400 }).unwrap();
        // type 3 code 4, then two unused bytes and the next-hop MTU
        assert_eq!(packet[IPV4_HEADER_LEN], 3);
        assert_eq!(packet[IPV4_HEADER_LEN + 1], 4);
        assert_eq!(
            u16::from_be_bytes([packet[IPV4_HEADER_LEN + 6], packet[IPV4_HEADER_LEN + 7]]),
            1400
        );
        let packet = build(GATEWAY6, &invoking(true, 64), Reason::TooBig { mtu: 1400 }).unwrap();
        // type 2, and the MTU occupies the whole four-byte field
        assert_eq!(packet[IPV6_HEADER_LEN], 2);
        assert_eq!(
            u32::from_be_bytes(
                packet[IPV6_HEADER_LEN + 4..IPV6_HEADER_LEN + 8]
                    .try_into()
                    .unwrap()
            ),
            1400
        );
    }

    #[test]
    fn crossing_families_is_refused() {
        assert!(build(GATEWAY4, &invoking(true, 64), Reason::Expired).is_err());
        assert!(build(GATEWAY6, &invoking(false, 64), Reason::Expired).is_err());
        assert!(build(GATEWAY4, &[], Reason::Expired).is_err());
    }
}
