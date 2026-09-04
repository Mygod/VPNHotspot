//! One-pass normalization of an IPv6 packet's header prefix.
//!
//! [RFC 8200 section 4.1](https://www.rfc-editor.org/rfc/rfc8200.html#section-4.1) supplies no header-count
//! limit, so one scan validates and measures the prefix before one allocation and copy. Atomic Fragment
//! headers are consumed here to preserve [RFC 6946 section
//! 4](https://www.rfc-editor.org/rfc/rfc6946.html#section-4) isolation; only genuine fragmentation reaches
//! [crate::shared::reassembly].
use etherparse::{IpNumber, Ipv6FragmentHeaderSlice, Ipv6HeaderSlice, Ipv6RawExtHeaderSlice};

use crate::shared::packet_writer::IPV6_HEADER_LEN;
use crate::shared::udp_wire::Reject;

/// What the one scan left of the packet.
#[derive(Debug, PartialEq, Eq)]
pub enum Walked {
    /// Nothing consumed; the caller retains the original bytes.
    None,
    /// The whole consumed prefix is gone and the header that followed it is promoted.
    Stripped(Vec<u8>),
}

/// One packet's normalized prefix, and where the scan stopped.
#[derive(Debug, PartialEq, Eq)]
pub struct Normalized {
    pub walked: Walked,
    /// Whether the result begins with a genuine or too-truncated-to-classify Fragment header.
    pub fragmenting: bool,
    /// Prefix headers consumed by this scan.
    pub consumed: usize,
}

/// Walks one IPv6 packet's header prefix and removes it.
pub fn walk(packet: &[u8]) -> Result<Normalized, Reject> {
    let Ok(header) = Ipv6HeaderSlice::from_slice(packet) else {
        return Ok(Normalized {
            walked: Walked::None,
            fragmenting: false,
            consumed: 0,
        });
    };
    let mut next = header.next_header();
    let mut offset = IPV6_HEADER_LEN;
    let mut consumed = 0usize;
    let fragmenting = loop {
        match next {
            IpNumber::IPV6_FRAGMENTATION_HEADER => {
                let Ok(fragment) = Ipv6FragmentHeaderSlice::from_slice(&packet[offset..]) else {
                    // Leave truncated fragments to reassembly's common rejection path.
                    break true;
                };
                if fragment.more_fragments() || fragment.fragment_offset().value() != 0 {
                    // Reassembly owns genuine fragmentation from here.
                    break true;
                }
                // Consume atomic headers without touching reassembly state.
                next = fragment.next_header();
                // The validated eight-byte slice strictly advances the walk.
                offset += fragment.slice().len();
                consumed += 1;
                continue;
            }
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
            _ => break false,
        }
        // RFC 8200 permits Hop-by-Hop only immediately after the IPv6 header.
        if next == IpNumber::IPV6_HEADER_HOP_BY_HOP && offset != IPV6_HEADER_LEN {
            return Err(Reject::Malformed("IPv6 hop-by-hop header is not first"));
        }
        // AH and ESP were rejected before applying this extension-length format.
        let extension = Ipv6RawExtHeaderSlice::from_slice(&packet[offset..]).map_err(|_| {
            if packet.len() < offset + 2 {
                Reject::Malformed("IPv6 extension header does not fit")
            } else {
                Reject::Malformed("IPv6 extension header runs past the packet")
            }
        })?;
        if next == IpNumber::IPV6_ROUTE_HEADER {
            // A nonzero Segments Left requests source routing.
            if extension.slice()[3] != 0 {
                return Err(Reject::Malformed("IPv6 source routing is not carried"));
            }
        }
        next = extension.next_header();
        // The validated slice strictly advances the finite packet walk.
        offset += extension.slice().len();
        consumed += 1;
    };
    if offset == IPV6_HEADER_LEN {
        // Preserve an untouched fragment exactly.
        return Ok(Normalized {
            walked: Walked::None,
            fragmenting,
            consumed,
        });
    }
    // Allocate once after the scan determines the retained bytes.
    let payload = packet.len() - offset;
    let length = u16::try_from(payload)
        .map_err(|_| Reject::Malformed("IPv6 payload length is impossible"))?;
    let mut stripped = Vec::with_capacity(IPV6_HEADER_LEN + payload);
    stripped.extend_from_slice(&packet[..IPV6_HEADER_LEN]);
    stripped[6] = next.0;
    stripped[4..6].copy_from_slice(&length.to_be_bytes());
    stripped.extend_from_slice(&packet[offset..]);
    Ok(Normalized {
        walked: Walked::Stripped(stripped),
        fragmenting,
        consumed,
    })
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Instant;

    use super::*;
    use crate::shared::classify::{classify, Classified, Drop, Principal};
    use crate::shared::reassembly;
    use crate::shared::udp_wire::{self, build_reply};

    const HOP_BY_HOP: u8 = 0;
    const ROUTING: u8 = 43;
    const FRAGMENT: u8 = 44;
    const DESTINATION: u8 = 60;
    const AH: u8 = 51;
    const ESP: u8 = 50;

    /// Prepends `(kind, length units, third byte)` headers to UDP.
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

    fn options(next: u8) -> [u8; 8] {
        [next, 0, 0, 0, 0, 0, 0, 0]
    }

    /// Builds outer options, fragmentation, and inner options; optionally nested.
    fn doubly_wrapped(destination: SocketAddr, payload: &[u8], nested: bool) -> Vec<Vec<u8>> {
        let datagram = build_reply(
            "[2001:db8:1::2]:40000".parse().unwrap(),
            destination,
            64,
            None,
            payload,
        )
        .unwrap();
        let mut fragmentable = options(if nested { FRAGMENT } else { datagram[6] }).to_vec();
        if nested {
            // Offset zero with more fragments set creates a retained reassembly context.
            fragmentable.extend_from_slice(&[datagram[6], 0, 0, 1, 0x9a, 0xbc, 0xde, 0xf1]);
        }
        fragmentable.extend_from_slice(&datagram[IPV6_HEADER_LEN..]);
        let split = 16;
        assert!(
            split < fragmentable.len(),
            "the split has to leave two real fragments"
        );
        [(0usize, split, true), (split, fragmentable.len(), false)]
            .into_iter()
            .map(|(offset, end, more)| {
                let mut body = options(FRAGMENT).to_vec();
                body.push(DESTINATION);
                body.push(0);
                body.extend_from_slice(&(((offset as u16) & !7) | u16::from(more)).to_be_bytes());
                body.extend_from_slice(&0x9abc_def0u32.to_be_bytes());
                body.extend_from_slice(&fragmentable[offset..end]);
                let mut fragment = datagram[..IPV6_HEADER_LEN].to_vec();
                fragment[6] = DESTINATION;
                fragment[4..6].copy_from_slice(&(body.len() as u16).to_be_bytes());
                fragment.extend_from_slice(&body);
                fragment
            })
            .collect()
    }

    fn table() -> reassembly::Table {
        reassembly::Table::new()
    }

    /// Builds one Fragment-header packet with the common test endpoints.
    fn fragmented(identification: u32, offset: usize, more: bool, body: &[u8]) -> Vec<u8> {
        let datagram = build_reply(
            "[2001:db8:1::2]:40000".parse().unwrap(),
            "[2606:4700::1111]:443".parse().unwrap(),
            64,
            None,
            b"unused",
        )
        .unwrap();
        let mut packet = datagram[..IPV6_HEADER_LEN].to_vec();
        packet[6] = FRAGMENT;
        packet[4..6].copy_from_slice(&((8 + body.len()) as u16).to_be_bytes());
        packet.push(datagram[6]);
        packet.push(0);
        packet.extend_from_slice(&(((offset as u16) & !7) | u16::from(more)).to_be_bytes());
        packet.extend_from_slice(&identification.to_be_bytes());
        packet.extend_from_slice(body);
        packet
    }

    /// Work performed by one rewrite lineage.
    #[derive(Default, Debug, PartialEq, Eq)]
    struct Cost {
        /// One scan per surviving rewrite round.
        scans: usize,
        /// How many prefix headers those scans consumed between them.
        consumed: usize,
        /// How many times reassembly was asked to hold or complete something.
        reassemblies: usize,
    }

    /// Models the parse/normalize/reassemble loop in `Dispatch::accept`.
    fn deliver(
        packet: &[u8],
        table: &mut reassembly::Table,
        now: Instant,
        cost: &mut Cost,
    ) -> Option<Vec<u8>> {
        let mut current = packet.to_vec();
        loop {
            let fragmented = match udp_wire::parse(&current) {
                Ok(_) => return Some(current),
                Err(Reject::Fragmented) => true,
                Err(Reject::Extended) => false,
                Err(e) => panic!("a transport rejected the packet as {e:?}"),
            };
            cost.scans += 1;
            let normalized = walk(&current).expect("the chain walks");
            cost.consumed += normalized.consumed;
            let produced = match normalized.walked {
                Walked::Stripped(stripped) if normalized.fragmenting => {
                    cost.reassemblies += 1;
                    held(table, &stripped, now)
                }
                Walked::Stripped(stripped) => Some(stripped),
                Walked::None if fragmented => {
                    cost.reassemblies += 1;
                    held(table, &current, now)
                }
                Walked::None => return None,
            };
            current = produced?;
        }
    }

    fn held(table: &mut reassembly::Table, packet: &[u8], now: Instant) -> Option<Vec<u8>> {
        match table.accept(packet, now) {
            Ok(reassembly::Accepted::Pending) => None,
            Ok(reassembly::Accepted::Complete(whole)) => Some(whole),
            refused => panic!("reassembly refused a conforming fragment: {refused:?}"),
        }
    }

    #[test]
    fn a_chain_is_removed_and_the_transport_promoted() {
        for chain in [
            vec![(DESTINATION, 0, 0)],
            vec![(HOP_BY_HOP, 0, 0)],
            vec![(HOP_BY_HOP, 0, 0), (DESTINATION, 1, 0)],
            vec![(HOP_BY_HOP, 0, 0), (ROUTING, 0, 0), (DESTINATION, 0, 0)],
        ] {
            let packet = wrapped(&chain);
            assert_eq!(udp_wire::parse(&packet), Err(Reject::Extended), "{chain:?}");
            let normalized = walk(&packet).expect("the chain walks");
            assert_eq!(normalized.consumed, chain.len(), "{chain:?}");
            assert!(!normalized.fragmenting, "{chain:?}");
            let Walked::Stripped(stripped) = normalized.walked else {
                panic!("{chain:?} should strip");
            };
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
        for bytes in [packet.as_slice(), &[0x45, 0, 0, 20], &[]] {
            assert_eq!(
                walk(bytes),
                Ok(Normalized {
                    walked: Walked::None,
                    fragmenting: false,
                    consumed: 0,
                })
            );
        }
    }

    #[test]
    fn a_genuine_fragment_header_ends_the_scan_and_stays() {
        let packet = wrapped(&[(DESTINATION, 0, 0), (FRAGMENT, 0, 1)]);
        let normalized = walk(&packet).expect("the part before it strips");
        assert!(normalized.fragmenting);
        assert_eq!(
            normalized.consumed, 1,
            "the Fragment header is not consumed"
        );
        let Walked::Stripped(stripped) = normalized.walked else {
            panic!("should strip the part before the fragment header");
        };
        assert_eq!(stripped[6], FRAGMENT);
        assert_eq!(udp_wire::parse(&stripped), Err(Reject::Fragmented));

        assert_eq!(
            walk(&wrapped(&[(FRAGMENT, 0, 1)])),
            Ok(Normalized {
                walked: Walked::None,
                fragmenting: true,
                consumed: 0,
            }),
            "a fragment with nothing in front of it is handed to reassembly untouched"
        );
    }

    #[test]
    fn an_atomic_fragment_header_is_consumed_rather_than_reassembled() {
        let mut table = table();
        let mut cost = Cost::default();
        let delivered = deliver(
            &wrapped(&[(FRAGMENT, 0, 0)]),
            &mut table,
            Instant::now(),
            &mut cost,
        )
        .expect("an atomic fragment is already a whole datagram");
        assert_eq!(
            udp_wire::parse(&delivered)
                .expect("the promoted datagram parses")
                .payload,
            b"payload"
        );
        assert_eq!(
            cost,
            Cost {
                scans: 1,
                consumed: 1,
                reassemblies: 0,
            },
            "RFC 6946 says process it as unfragmented, so reassembly never sees it"
        );
    }

    #[test]
    fn a_long_atomic_chain_is_one_scan_with_one_copy_and_no_count_refusal() {
        for depth in [2usize, 64, 1_000] {
            let chain: Vec<_> = std::iter::repeat_n((FRAGMENT, 0, 0), depth).collect();
            let mut table = table();
            let mut cost = Cost::default();
            let delivered = deliver(&wrapped(&chain), &mut table, Instant::now(), &mut cost)
                .unwrap_or_else(|| panic!("a {depth}-deep atomic chain delivers its transport"));
            assert_eq!(
                udp_wire::parse(&delivered)
                    .expect("the promoted datagram parses")
                    .payload,
                b"payload",
                "depth {depth}"
            );
            assert_eq!(
                cost,
                Cost {
                    scans: 1,
                    consumed: depth,
                    reassemblies: 0,
                },
                "depth {depth}: one scan for the whole chain, whatever its depth"
            );
            assert!(
                table.describe().starts_with("0 contexts"),
                "depth {depth}: reassembly holds nothing"
            );
        }
    }

    #[test]
    fn atomic_headers_mixed_into_a_chain_are_consumed_by_the_same_scan() {
        let chain: Vec<_> = std::iter::repeat_n(
            [(DESTINATION, 0, 0), (FRAGMENT, 0, 0), (ROUTING, 0, 0)],
            128,
        )
        .flatten()
        .collect();
        let mut table = table();
        let mut cost = Cost::default();
        let delivered = deliver(&wrapped(&chain), &mut table, Instant::now(), &mut cost)
            .expect("a mixed chain delivers its transport");
        assert_eq!(
            udp_wire::parse(&delivered)
                .expect("the promoted datagram parses")
                .payload,
            b"payload"
        );
        assert_eq!(
            cost,
            Cost {
                scans: 1,
                consumed: chain.len(),
                reassemblies: 0,
            }
        );
    }

    #[test]
    fn a_chain_on_both_sides_of_a_fragment_header_still_delivers() {
        let destination: SocketAddr = "[fd00::53]:53".parse().unwrap();
        let mut table = table();
        let now = Instant::now();
        let mut cost = Cost::default();
        let mut delivered = None;
        // Only the completing fragment continues the lineage.
        for fragment in doubly_wrapped(destination, b"a query for the resolver", false) {
            assert_eq!(
                classify(&fragment, &[destination.ip()]),
                Classified::Accepted {
                    principal: Principal::Dns,
                    provisional: true
                }
            );
            if let Some(whole) = deliver(&fragment, &mut table, now, &mut cost) {
                delivered = Some(whole);
            }
        }
        let bare = delivered.expect("the last fragment completes the datagram");
        assert_eq!(
            classify(&bare, &[destination.ip()]),
            Classified::Accepted {
                principal: Principal::Dns,
                provisional: false
            }
        );
        assert_eq!(
            udp_wire::parse(&bare).expect("the datagram parses").payload,
            b"a query for the resolver"
        );
        assert_eq!(
            cost,
            Cost {
                // Two fragments plus the completed inner scan.
                scans: 3,
                consumed: 3,
                reassemblies: 2,
            },
            "genuine fragmentation is the only thing that costs a reassembly round"
        );
    }

    #[test]
    fn a_chain_on_both_sides_of_a_fragment_header_is_still_no_route_past_the_resolver() {
        let destination: SocketAddr = "[fd00::53]:443".parse().unwrap();
        let mut table = table();
        let now = Instant::now();
        let mut cost = Cost::default();
        let mut delivered = None;
        for fragment in doubly_wrapped(destination, b"not a query", false) {
            if let Some(whole) = deliver(&fragment, &mut table, now, &mut cost) {
                delivered = Some(whole);
            }
        }
        assert_eq!(
            classify(
                &delivered.expect("the last fragment completes the datagram"),
                &[destination.ip()]
            ),
            Classified::Dropped(Drop::Reserved)
        );
    }

    #[test]
    fn a_nested_genuine_fragment_reaches_reassembly() {
        let destination: SocketAddr = "[fd00::53]:53".parse().unwrap();
        let now = Instant::now();
        let mut table = table();
        let mut cost = Cost::default();
        let mut delivered = None;
        for fragment in doubly_wrapped(destination, b"a query for the resolver", true) {
            if let Some(whole) = deliver(&fragment, &mut table, now, &mut cost) {
                delivered = Some(whole);
            }
        }
        assert_eq!(
            delivered, None,
            "the inner fragment is a first fragment with more to come, so nothing is whole yet"
        );
        assert!(
            table.next_deadline().is_some(),
            "and the inner reassembly context is retained"
        );
        assert_eq!(
            cost.reassemblies, 3,
            "two outer fragments and the inner one they completed"
        );
    }

    #[test]
    fn an_atomic_fragment_cannot_disturb_a_context_sharing_its_identification() {
        // Atomic and genuine fragments deliberately share a reassembly key.
        const SHARED: u32 = 0x0bad_1dea;
        let whole = build_reply(
            "[2001:db8:1::2]:40000".parse().unwrap(),
            "[2606:4700::1111]:443".parse().unwrap(),
            64,
            None,
            b"the genuinely fragmented datagram",
        )
        .unwrap();
        let body = &whole[IPV6_HEADER_LEN..];
        let split = 16;
        assert!(split < body.len() && split.is_multiple_of(8));

        let atomic = build_reply(
            "[2001:db8:1::2]:40000".parse().unwrap(),
            "[2606:4700::1111]:443".parse().unwrap(),
            64,
            None,
            b"an atomic datagram",
        )
        .unwrap();

        let now = Instant::now();
        let mut table = table();
        let mut cost = Cost::default();

        // Genuine fragmentation opens a context.
        assert_eq!(
            deliver(
                &fragmented(SHARED, 0, true, &body[..split]),
                &mut table,
                now,
                &mut cost
            ),
            None,
            "the datagram is not whole yet"
        );
        assert_eq!(cost.reassemblies, 1);
        let held = table.describe();

        // The atomic packet bypasses that context.
        let delivered = deliver(
            &fragmented(SHARED, 0, false, &atomic[IPV6_HEADER_LEN..]),
            &mut table,
            now,
            &mut cost,
        )
        .expect("an atomic fragment is already a whole datagram");
        assert_eq!(
            udp_wire::parse(&delivered)
                .expect("the promoted datagram parses")
                .payload,
            b"an atomic datagram"
        );
        assert_eq!(
            cost.reassemblies, 1,
            "the atomic packet reached no reassembly context"
        );
        assert_eq!(
            table.describe(),
            held,
            "and left the genuine one exactly as it was"
        );

        // The genuine datagram still completes.
        let completed = deliver(
            &fragmented(SHARED, split, false, &body[split..]),
            &mut table,
            now,
            &mut cost,
        )
        .expect("the last genuine fragment completes its datagram");
        assert_eq!(
            udp_wire::parse(&completed)
                .expect("the reassembled datagram parses")
                .payload,
            b"the genuinely fragmented datagram"
        );
        assert!(table.describe().starts_with("0 contexts"));
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
            packet.truncate(IPV6_HEADER_LEN + 1);
            assert_eq!(walk(&packet), Err(expected), "kind {kind}");
        }
    }

    #[test]
    fn a_truncated_fragment_header_is_left_to_reassembly() {
        let mut packet = wrapped(&[(DESTINATION, 0, 0), (FRAGMENT, 0, 0)]);
        packet.truncate(IPV6_HEADER_LEN + 8 + 1);
        let normalized = walk(&packet).expect("the part before it strips");
        assert!(
            normalized.fragmenting,
            "too short to tell whether it is atomic, so the one owner of that decision gets it"
        );
        let Walked::Stripped(stripped) = normalized.walked else {
            panic!("should stop at the fragment boundary");
        };
        assert_eq!(stripped[6], FRAGMENT);
        assert_eq!(stripped.len(), IPV6_HEADER_LEN + 1);
        assert!(matches!(
            table().accept(&stripped, Instant::now()),
            Err(reassembly::Reject::Malformed(_))
        ));
    }

    #[test]
    fn the_fragment_header_reserved_byte_is_not_read_as_a_length() {
        let mut packet = wrapped(&[(DESTINATION, 0, 0), (FRAGMENT, 0, 0)]);
        // A Fragment header's second byte is reserved, not a generic extension length.
        packet[IPV6_HEADER_LEN + 8 + 1] = 0xff;
        let normalized = walk(&packet).expect("the chain walks");
        assert_eq!(normalized.consumed, 2);
        assert!(!normalized.fragmenting);
        let Walked::Stripped(stripped) = normalized.walked else {
            panic!("both headers should be consumed");
        };
        assert_eq!(
            udp_wire::parse(&stripped)
                .expect("the promoted datagram parses")
                .payload,
            b"payload"
        );
    }

    #[test]
    fn source_routing_is_refused_rather_than_performed() {
        assert_eq!(
            walk(&wrapped(&[(ROUTING, 0, 1)])),
            Err(Reject::Malformed("IPv6 source routing is not carried"))
        );
        assert!(matches!(
            walk(&wrapped(&[(ROUTING, 0, 0)])),
            Ok(Normalized {
                walked: Walked::Stripped(_),
                ..
            })
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
    fn a_long_chain_is_bounded_by_the_packet_and_reaches_its_transport() {
        let chain: Vec<_> = std::iter::repeat_n((DESTINATION, 0, 0), 1_000).collect();
        let normalized = walk(&wrapped(&chain)).expect("a complete chain is walked");
        assert_eq!(normalized.consumed, 1_000, "no count refuses a chain");
        let Walked::Stripped(stripped) = normalized.walked else {
            panic!("a complete chain should be walked to its transport");
        };
        assert_eq!(
            udp_wire::parse(&stripped)
                .expect("the promoted datagram parses")
                .payload,
            b"payload"
        );
    }

    #[test]
    fn a_truncated_chain_is_refused() {
        let mut packet = wrapped(&[(DESTINATION, 0, 0)]);
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
}
