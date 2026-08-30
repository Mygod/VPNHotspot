use etherparse::{IpNumber, Ipv6HeaderSlice, Ipv6RawExtHeaderSlice};

use crate::shared::packet_writer::IPV6_HEADER_LEN;
use crate::shared::udp_wire::Reject;

/// How many extension headers one chain may contain.
const MAX_HEADERS: usize = 6;

/// Maximum rewrites per read: outer extension chain, reassembly, then inner extension chain.
pub const REWRITES: usize = 3;

/// Runs at most [REWRITES] actions, charging before invoking each action.
pub struct Budget(usize);

impl Default for Budget {
    fn default() -> Self {
        Self(REWRITES)
    }
}

impl Budget {
    /// Runs `rewrite` only when budget remains.
    pub fn spend<T>(&mut self, rewrite: impl FnOnce() -> T) -> Option<T> {
        self.0 = self.0.checked_sub(1)?;
        Some(rewrite())
    }
}

/// What walking a chain produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Walked {
    /// There was no chain, so the packet is already the shape a transport parse expects.
    None,
    /// The chain was removed and the header that followed it promoted.
    Stripped(Vec<u8>),
}

/// Walks one IPv6 packet's extension chain and removes it.
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
    use std::net::{IpAddr, SocketAddr};
    use std::time::Instant;

    use super::*;
    use crate::shared::admission::{Admission, Class, Lease, Request, Totals};
    use crate::shared::classify::{classify, Classified, Drop, Principal};
    use crate::shared::reassembly;
    use crate::shared::udp_wire::{self, build_reply};

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

    const HOP_BY_HOP: u8 = 0;
    const ROUTING: u8 = 43;
    const FRAGMENT: u8 = 44;
    const DESTINATION: u8 = 60;
    const AH: u8 = 51;
    const ESP: u8 = 50;

    fn options(next: u8) -> [u8; 8] {
        [next, 0, 0, 0, 0, 0, 0, 0]
    }

    /// Builds outer options, a Fragment header, and inner options. `nested` adds a fourth wrapper.
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

    fn table() -> (reassembly::Table, Admission, Lease) {
        let mut admission = Admission::new(Totals {
            admission_id: 1,
            record_total: 64,
            dns_record_floor: 0,
            byte_total: 8 << 20,
            reserved_byte_floor: 1 << 20,
            fragment_cap: 1 << 20,
            byte_only_owners: 4,
        })
        .expect("the fixture totals hold their own accounting");
        let lease = admission
            .reserve(Request::bytes(
                reassembly::Table::footprint(4).expect("fits"),
                Class::General,
            ))
            .expect("granted");
        (reassembly::Table::with_capacity(4), admission, lease)
    }

    /// Applies the same gated rewrite sequence as `Dispatch::accept`.
    fn rewritten(
        fragments: &[Vec<u8>],
        virtual_addresses: &[IpAddr],
        table: &mut reassembly::Table,
        admission: &mut Admission,
        lease: &Lease,
        now: Instant,
    ) -> (Vec<u8>, Budget) {
        let provisional = Classified::Accepted {
            principal: Principal::Dns,
            provisional: true,
        };
        let mut whole = None;
        // Each fragment is a separate read; only the completing fragment continues the lineage.
        let mut rewrites = Budget::default();
        for fragment in fragments {
            rewrites = Budget::default();
            assert_eq!(classify(fragment, virtual_addresses), provisional);
            assert_eq!(udp_wire::parse(fragment), Err(Reject::Extended));
            let Some(walked) = rewrites.spend(|| walk(fragment)) else {
                panic!("stripping that chain is the first rewrite a read is owed");
            };
            let Ok(Walked::Stripped(unwrapped)) = walked else {
                panic!("the chain in front of the Fragment header strips");
            };
            assert_eq!(classify(&unwrapped, virtual_addresses), provisional);
            assert_eq!(udp_wire::parse(&unwrapped), Err(Reject::Fragmented));
            let Some(held) = rewrites.spend(|| table.accept(&unwrapped, now, admission, lease))
            else {
                panic!("holding the fragment is the second rewrite a read is owed");
            };
            match held {
                Ok(reassembly::Accepted::Pending) => {}
                Ok(reassembly::Accepted::Complete(assembled)) => whole = Some(assembled),
                refused => panic!("reassembly refused a conforming fragment: {refused:?}"),
            }
        }
        let whole = whole.expect("the last fragment completes the datagram");
        assert_eq!(classify(&whole, virtual_addresses), provisional);
        assert_eq!(udp_wire::parse(&whole), Err(Reject::Extended));
        let Some(walked) = rewrites.spend(|| walk(&whole)) else {
            panic!("stripping that chain is the third rewrite a read is owed");
        };
        let Ok(Walked::Stripped(bare)) = walked else {
            panic!("the chain behind the Fragment header strips");
        };
        (bare, rewrites)
    }

    #[test]
    fn a_chain_on_both_sides_of_a_fragment_header_still_delivers() {
        let destination: SocketAddr = "[fd00::53]:53".parse().unwrap();
        let (mut table, mut admission, lease) = table();
        let (bare, mut rewrites) = rewritten(
            &doubly_wrapped(destination, b"a query for the resolver", false),
            &[destination.ip()],
            &mut table,
            &mut admission,
            &lease,
            Instant::now(),
        );
        assert!(
            rewrites.spend(|| ()).is_none(),
            "three rewrites is all a read is owed"
        );
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
    }

    #[test]
    fn a_chain_on_both_sides_of_a_fragment_header_is_still_no_route_past_the_resolver() {
        let destination: SocketAddr = "[fd00::53]:443".parse().unwrap();
        let (mut table, mut admission, lease) = table();
        let (bare, mut rewrites) = rewritten(
            &doubly_wrapped(destination, b"not a query", false),
            &[destination.ip()],
            &mut table,
            &mut admission,
            &lease,
            Instant::now(),
        );
        assert!(
            rewrites.spend(|| ()).is_none(),
            "three rewrites is all a read is owed"
        );
        assert_eq!(
            classify(&bare, &[destination.ip()]),
            Classified::Dropped(Drop::Reserved)
        );
    }

    #[test]
    fn a_fragment_header_behind_every_rewrite_buys_no_reassembly_context() {
        let destination: SocketAddr = "[fd00::53]:53".parse().unwrap();
        let now = Instant::now();
        let (mut probe, mut charged, probe_lease) = table();
        let (mut table, mut admission, lease) = table();
        let (bare, mut rewrites) = rewritten(
            &doubly_wrapped(destination, b"a query for the resolver", true),
            &[destination.ip()],
            &mut table,
            &mut admission,
            &lease,
            now,
        );
        assert_eq!(
            classify(&bare, &[destination.ip()]),
            Classified::Accepted {
                principal: Principal::Dns,
                provisional: false
            }
        );
        assert_eq!(udp_wire::parse(&bare), Err(Reject::Fragmented));
        let mut asked = false;
        let held = rewrites.spend(|| {
            asked = true;
            table.accept(&bare, now, &mut admission, &lease)
        });
        assert!(held.is_none(), "a fourth rewrite is not owed");
        assert!(!asked, "so the reassembly table was never asked for one");
        assert_eq!(table.next_deadline(), None);
        assert_eq!(admission.fragment_bytes_charged(), 0);
        // Without the budget gate, the same fragment would retain a charged context.
        assert_eq!(
            probe.accept(&bare, now, &mut charged, &probe_lease),
            Ok(reassembly::Accepted::Pending)
        );
        assert!(probe.next_deadline().is_some());
        assert_ne!(charged.fragment_bytes_charged(), 0);
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
            let Ok(Walked::Stripped(stripped)) = walk(&packet) else {
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
        assert_eq!(walk(&packet), Ok(Walked::None));
        assert_eq!(walk(&[0x45, 0, 0, 20]), Ok(Walked::None));
        assert_eq!(walk(&[]), Ok(Walked::None));
    }

    #[test]
    fn a_fragment_header_ends_the_walk_and_stays() {
        let packet = wrapped(&[(DESTINATION, 0, 0), (FRAGMENT, 0, 0)]);
        let Ok(Walked::Stripped(stripped)) = walk(&packet) else {
            panic!("should strip the part before the fragment header");
        };
        assert_eq!(stripped[6], FRAGMENT);
        assert_eq!(udp_wire::parse(&stripped), Err(Reject::Fragmented));
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
        assert_eq!(
            walk(&wrapped(&[(ROUTING, 0, 1)])),
            Err(Reject::Malformed("IPv6 source routing is not carried"))
        );
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
        let chain: Vec<_> = std::iter::repeat_n((DESTINATION, 0, 0), MAX_HEADERS).collect();
        assert!(matches!(walk(&wrapped(&chain)), Ok(Walked::Stripped(_))));
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

    #[test]
    fn the_fragment_header_length_is_never_read() {
        let mut packet = wrapped(&[(DESTINATION, 0, 0), (FRAGMENT, 0, 0)]);
        packet[IPV6_HEADER_LEN + 8 + 1] = 0xff;
        let Ok(Walked::Stripped(stripped)) = walk(&packet) else {
            panic!("should strip the chain before the fragment header");
        };
        assert_eq!(stripped[6], FRAGMENT);
        assert_eq!(udp_wire::parse(&stripped), Err(Reject::Fragmented));
    }
}
