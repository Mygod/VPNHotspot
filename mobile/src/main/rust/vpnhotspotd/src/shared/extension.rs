//! The bounded walk over an IPv6 extension-header chain.
//!
//! The transport parsers each expect their header at a fixed offset, so a client that sends any extension header
//! would otherwise have its traffic counted and dropped. This removes the chain and promotes the transport,
//! which is the same trick reassembly uses: hand the strict parsers a shape they already understand rather than
//! teach each of them a second one.
//!
//! Removing rather than preserving is forced, not chosen. Egress goes out through a datagram socket, so the
//! kernel builds the IPv6 header and there is nowhere to put a chain even if it were kept - the same reason the
//! source address changes. Hop-by-Hop and Routing options are for hops along the way and lose their meaning at
//! a relay that re-originates; Destination Options are the real loss, and it is a documented one.
//!
//! Source-routing and misplaced Hop-by-Hop chains are refused rather than walked. A Routing header with
//! segments left is source routing, which RFC 5095 deprecates outright and which a relay must not carry out on
//! a client's behalf. A Hop-by-Hop header anywhere but first violates RFC 8200's ordering, and accepting it
//! would mean accepting a chain whose meaning two readers could disagree about. AH, ESP, and extension types
//! outside Hop-by-Hop, Destination Options, Routing, and Fragment are unsupported.
//!
//! A Fragment header ends the walk instead of being removed, and the chain before it is still stripped. What
//! comes back is a packet whose next header is the Fragment header, which is exactly what reassembly expects -
//! so a fragmented packet wrapped in extension headers is handled by both in turn rather than by either alone.

use etherparse::{IpNumber, Ipv6HeaderSlice, Ipv6RawExtHeaderSlice};

use crate::shared::packet_writer::IPV6_HEADER_LEN;
use crate::shared::udp_wire::Reject;

/// How many extension headers one chain may contain.
///
/// RFC 8200 requires Hop-by-Hop to appear first and gives no reason for a removable header to repeat. Six
/// removable headers is already more than a conforming chain needs, so refusing past this bounds the work per
/// packet without refusing one this relay can usefully carry.
///
/// https://www.rfc-editor.org/rfc/rfc8200#section-4.1
const MAX_HEADERS: usize = 6;

/// What walking a chain produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Walked {
    /// There was no chain, so the packet is already the shape a transport parse expects.
    None,
    /// The chain was removed and the header that followed it promoted.
    Stripped(Vec<u8>),
}

/// Walks one IPv6 packet's extension chain and removes it.
///
/// Only ever called for a packet a transport parse has already refused as extended, so an IPv4 packet or one
/// with no chain comes back [Walked::None] rather than being an error.
pub fn walk(packet: &[u8]) -> Result<Walked, Reject> {
    let Ok(header) = Ipv6HeaderSlice::from_slice(packet) else {
        return Ok(Walked::None);
    };
    let mut next = header.next_header();
    let mut offset = IPV6_HEADER_LEN;
    let mut walked = 0;
    loop {
        match next {
            // Kept rather than removed: reassembly owns everything from here, and it is what promotes whatever
            // the Fragment header points at once the datagram is whole. Breaking here also means its reserved
            // byte is never mistaken for a generic extension-header length.
            IpNumber::IPV6_FRAGMENTATION_HEADER => break,
            IpNumber::IPV6_HEADER_HOP_BY_HOP
            | IpNumber::IPV6_DESTINATION_OPTIONS
            | IpNumber::IPV6_ROUTE_HEADER => {}
            IpNumber::AUTHENTICATION_HEADER => {
                return Err(Reject::Malformed(
                    "IPv6 authentication header is not carried",
                ));
            }
            IpNumber::ENCAPSULATING_SECURITY_PAYLOAD => {
                return Err(Reject::Malformed("IPv6 encrypted payload is not carried"));
            }
            _ if next.is_ipv6_ext_header_value() => {
                return Err(Reject::Malformed("unsupported IPv6 extension header"));
            }
            _ => break,
        }
        if walked == MAX_HEADERS {
            return Err(Reject::Malformed("IPv6 extension chain is too long"));
        }
        // Hop-by-Hop is only ever the first, per RFC 8200. Anywhere else and the chain is one whose meaning two
        // readers could disagree about, which is not a chain to repeat.
        if next == IpNumber::IPV6_HEADER_HOP_BY_HOP && offset != IPV6_HEADER_LEN {
            return Err(Reject::Malformed("IPv6 hop-by-hop header is not first"));
        }
        // This typed slice applies the eight-octet length format only to the three header types above. AH has
        // a different formula and ESP has no leading next-header field, which is why both were rejected first.
        let extension = Ipv6RawExtHeaderSlice::from_slice(&packet[offset..]).map_err(|_| {
            if packet.len() < offset + 2 {
                Reject::Malformed("IPv6 extension header does not fit")
            } else {
                Reject::Malformed("IPv6 extension header runs past the packet")
            }
        })?;
        if next == IpNumber::IPV6_ROUTE_HEADER {
            // Segments left is the third byte. Non-zero means the client is asking to be routed through
            // somewhere of its choosing, which RFC 5095 deprecates and which a relay must not perform for it.
            if extension.slice()[3] != 0 {
                return Err(Reject::Malformed("IPv6 source routing is not carried"));
            }
        }
        next = extension.next_header();
        offset += extension.slice().len();
        walked += 1;
    }
    if walked == 0 {
        // Nothing was removed, which for a fragment means reassembly should see it exactly as it arrived.
        return Ok(Walked::None);
    }
    let payload = packet.len() - offset;
    let length = u16::try_from(payload)
        .map_err(|_| Reject::Malformed("IPv6 payload length is impossible"))?;
    let mut stripped = Vec::with_capacity(IPV6_HEADER_LEN + payload);
    stripped.extend_from_slice(&packet[..IPV6_HEADER_LEN]);
    stripped[6] = next.0;
    stripped[4..6].copy_from_slice(&length.to_be_bytes());
    stripped.extend_from_slice(&packet[offset..]);
    Ok(Walked::Stripped(stripped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::udp_wire::{self, build_reply};

    /// A whole UDP datagram, then `chain` spliced in between its IPv6 header and its transport.
    fn wrapped(chain: &[(u8, usize, u8)]) -> Vec<u8> {
        let packet = build_reply(
            "[2001:db8:1::2]:40000".parse().unwrap(),
            "[2606:4700::1111]:443".parse().unwrap(),
            64,
            None,
            b"payload",
        )
        .unwrap();
        let mut headers = Vec::new();
        // each entry becomes one header pointing at whatever follows it
        for (index, (_, units, third)) in chain.iter().enumerate() {
            let next = chain.get(index + 1).map_or(packet[6], |(kind, _, _)| *kind);
            let mut header = vec![0u8; (units + 1) * 8];
            header[0] = next;
            header[1] = *units as u8;
            header[3] = *third;
            headers.extend_from_slice(&header);
        }
        let mut out = packet[..IPV6_HEADER_LEN].to_vec();
        out[6] = chain[0].0;
        let payload = headers.len() + packet.len() - IPV6_HEADER_LEN;
        out[4..6].copy_from_slice(&(payload as u16).to_be_bytes());
        out.extend_from_slice(&headers);
        out.extend_from_slice(&packet[IPV6_HEADER_LEN..]);
        out
    }

    const HOP_BY_HOP: u8 = 0;
    const ROUTING: u8 = 43;
    const FRAGMENT: u8 = 44;
    const DESTINATION: u8 = 60;
    const AH: u8 = 51;
    const ESP: u8 = 50;

    #[test]
    fn a_chain_is_removed_and_the_transport_promoted() {
        for chain in [
            vec![(DESTINATION, 0, 0)],
            vec![(HOP_BY_HOP, 0, 0)],
            vec![(HOP_BY_HOP, 0, 0), (DESTINATION, 1, 0)],
            vec![(HOP_BY_HOP, 0, 0), (ROUTING, 0, 0), (DESTINATION, 0, 0)],
        ] {
            let packet = wrapped(&chain);
            // the strict parse refuses it as extended before the walk
            assert_eq!(udp_wire::parse(&packet), Err(Reject::Extended), "{chain:?}");
            let Ok(Walked::Stripped(stripped)) = walk(&packet) else {
                panic!("{chain:?} should strip");
            };
            // and afterwards it parses as the ordinary datagram it always was
            let datagram = udp_wire::parse(&stripped).expect("stripped parses");
            assert_eq!(datagram.payload, b"payload", "{chain:?}");
            assert_eq!(datagram.hop_limit, 64, "{chain:?}");
        }
    }

    #[test]
    fn a_packet_with_no_chain_is_left_alone() {
        let packet = build_reply(
            "[2001:db8:1::2]:40000".parse().unwrap(),
            "[2606:4700::1111]:443".parse().unwrap(),
            64,
            None,
            b"payload",
        )
        .unwrap();
        assert_eq!(walk(&packet), Ok(Walked::None));
        // and neither is an IPv4 one, which has no chain to walk at all
        assert_eq!(walk(&[0x45, 0, 0, 20]), Ok(Walked::None));
        assert_eq!(walk(&[]), Ok(Walked::None));
    }

    #[test]
    fn a_fragment_header_ends_the_walk_and_stays() {
        // Reassembly owns the fragment header, so a chain in front of it is stripped and it is left in place.
        let packet = wrapped(&[(DESTINATION, 0, 0), (FRAGMENT, 0, 0)]);
        let Ok(Walked::Stripped(stripped)) = walk(&packet) else {
            panic!("should strip the part before the fragment header");
        };
        assert_eq!(stripped[6], FRAGMENT);
        // which is exactly what the parses report as fragmented, handing it to reassembly
        assert_eq!(udp_wire::parse(&stripped), Err(Reject::Fragmented));
        // and a bare fragment header is left untouched, so reassembly sees it as it arrived
        assert_eq!(walk(&wrapped(&[(FRAGMENT, 0, 0)])), Ok(Walked::None));
    }

    #[test]
    fn ah_and_esp_are_rejected_without_applying_the_generic_length_format() {
        for (kind, expected) in [
            (
                AH,
                Reject::Malformed("IPv6 authentication header is not carried"),
            ),
            (
                ESP,
                Reject::Malformed("IPv6 encrypted payload is not carried"),
            ),
        ] {
            let mut packet = wrapped(&[(DESTINATION, 0, 0)]);
            packet[6] = kind;
            // Deliberately not a valid header body: the unsupported kind is rejected before bytes with a
            // different or absent length format can be interpreted.
            packet.truncate(IPV6_HEADER_LEN + 1);
            assert_eq!(walk(&packet), Err(expected), "kind {kind}");
        }
    }

    #[test]
    fn fragment_body_is_not_parsed_by_the_extension_walker() {
        let mut packet = wrapped(&[(DESTINATION, 0, 0), (FRAGMENT, 0, 0)]);
        packet.truncate(IPV6_HEADER_LEN + 8 + 1);
        let Ok(Walked::Stripped(stripped)) = walk(&packet) else {
            panic!("should stop at the fragment boundary");
        };
        assert_eq!(stripped[6], FRAGMENT);
        assert_eq!(stripped.len(), IPV6_HEADER_LEN + 1);
    }

    #[test]
    fn source_routing_is_refused_rather_than_performed() {
        // segments left non-zero: the client is asking to be routed via somewhere of its choosing
        assert_eq!(
            walk(&wrapped(&[(ROUTING, 0, 1)])),
            Err(Reject::Malformed("IPv6 source routing is not carried"))
        );
        // with none left it is spent and merely has to be stepped over
        assert!(matches!(
            walk(&wrapped(&[(ROUTING, 0, 0)])),
            Ok(Walked::Stripped(_))
        ));
    }

    #[test]
    fn a_misplaced_hop_by_hop_header_is_refused() {
        assert_eq!(
            walk(&wrapped(&[(DESTINATION, 0, 0), (HOP_BY_HOP, 0, 0)])),
            Err(Reject::Malformed("IPv6 hop-by-hop header is not first"))
        );
    }

    #[test]
    fn an_overlong_chain_is_refused_before_it_is_walked() {
        let chain: Vec<_> = std::iter::repeat_n((DESTINATION, 0, 0), MAX_HEADERS + 1).collect();
        assert_eq!(
            walk(&wrapped(&chain)),
            Err(Reject::Malformed("IPv6 extension chain is too long"))
        );
        // exactly at the bound is still walked, so the limit refuses only what exceeds it
        let chain: Vec<_> = std::iter::repeat_n((DESTINATION, 0, 0), MAX_HEADERS).collect();
        assert!(matches!(walk(&wrapped(&chain)), Ok(Walked::Stripped(_))));
    }

    #[test]
    fn a_truncated_chain_is_refused() {
        let mut packet = wrapped(&[(DESTINATION, 0, 0)]);
        // claim a length the packet cannot hold
        packet[IPV6_HEADER_LEN + 1] = 0xff;
        assert_eq!(
            walk(&packet),
            Err(Reject::Malformed(
                "IPv6 extension header runs past the packet"
            ))
        );
        assert_eq!(
            walk(&packet[..IPV6_HEADER_LEN + 1]),
            Err(Reject::Malformed("IPv6 extension header does not fit"))
        );
    }

    #[test]
    fn the_fragment_header_length_is_never_read() {
        // A Fragment header has no length field: that byte is reserved. Ending the walk before reading one is
        // what keeps a nonsense value there from being believed, which is what this leaves behind to check.
        let mut packet = wrapped(&[(DESTINATION, 0, 0), (FRAGMENT, 0, 0)]);
        packet[IPV6_HEADER_LEN + 8 + 1] = 0xff;
        let Ok(Walked::Stripped(stripped)) = walk(&packet) else {
            panic!("should strip the chain before the fragment header");
        };
        assert_eq!(stripped[6], FRAGMENT);
        assert_eq!(udp_wire::parse(&stripped), Err(Reject::Fragmented));
    }
}
