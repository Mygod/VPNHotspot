use std::net::{IpAddr, SocketAddr};

use crate::shared::icmp_error::{self, Reason};
use crate::shared::udp_wire::build_reply;

/// The smallest MTU a router may report for IPv4. RFC 791 requires every host to handle 68 bytes, so a
/// smaller claim is not a path this daemon can describe - and a client that believed it would stop sending
/// anything useful.
const IPV4_MINIMUM_MTU: u32 = 68;

/// RFC 8200 section 5: IPv6's minimum link MTU. A Packet Too Big claiming less is either broken or hostile,
/// and either way the client must not cache it.
const IPV6_MINIMUM_MTU: u32 = 1280;

const ICMPV4_DESTINATION_UNREACHABLE: u8 = 3;
const ICMPV4_TIME_EXCEEDED: u8 = 11;
const ICMPV4_FRAGMENTATION_NEEDED: u8 = 4;
const ICMPV4_PARAMETER_PROBLEM: u8 = 12;
const ICMPV6_DESTINATION_UNREACHABLE: u8 = 1;
const ICMPV6_PACKET_TOO_BIG: u8 = 2;
const ICMPV6_TIME_EXCEEDED: u8 = 3;
const TTL_EXCEEDED_IN_TRANSIT: u8 = 0;

/// One ICMP error a *router* sent about a packet this daemon relayed, in the terms a translation needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reported {
    /// Who sent it. A repeated error must appear to come from here rather than from the interface, because that
    /// is what makes the daemon a hop in a traceroute instead of the end of one.
    pub remote: IpAddr,
    /// Where the offending packet was going, when the kernel says.
    pub destination: Option<SocketAddr>,
    /// The error's own received hop limit, required rather than defaulted.
    pub hop_limit: u8,
    pub icmp_type: u8,
    pub code: u8,
    /// Protocol-specific: the reported MTU for a too-big, the pointer for a parameter problem.
    pub info: u32,
    /// The offending packet's bytes from its transport header onward, as the kernel kept them.
    pub quoted: Quote,
}

/// How much of an offending packet is kept, which is everything any correlation reads.
pub const QUOTE_BYTES: usize = 8;

/// The kept prefix of an offending packet, inline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quote {
    bytes: [u8; QUOTE_BYTES],
    /// How much of [Quote::bytes] the kernel actually filled. A short quote is normal - a router may return
    /// less - and an empty one is what a correlation must not treat as a match.
    length: u8,
}

impl Quote {
    /// Keeps the first [QUOTE_BYTES] of what was read, dropping the rest.
    pub fn new(bytes: &[u8]) -> Self {
        let length = bytes.len().min(QUOTE_BYTES);
        let mut kept = [0u8; QUOTE_BYTES];
        kept[..length].copy_from_slice(&bytes[..length]);
        Quote {
            bytes: kept,
            // The min above is what makes this fit; u8 holds QUOTE_BYTES with room to spare.
            length: length as u8,
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.length as usize]
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
}

/// What the caller can prove about the packet an error describes, which is what decides how much of the error
/// may be repeated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Correlation {
    /// This client sent *to that address*. Enough for a claim about the route there.
    Address,
    /// This client sent *that exact datagram*, and this is the hop limit it used. Enough for a claim about one
    /// datagram, which is what the send history exists to establish.
    Datagram { hop_limit: u8 },
}

/// Why a remote's error is not repeated. Counted rather than logged: a remote chooses these, so one report per
/// error would be a flood it controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Untranslatable {
    /// A type and code this translator does not carry.
    Unsupported,
    /// A type whose claim is about one datagram, offered with only address-level proof. Not a refusal of the
    /// error so much as of the evidence: the same error with a matched send behind it is repeated.
    Uncorrelated,
    /// The right type, but carrying a value no path could have: an MTU below the family's minimum, or a
    /// reassembly timeout, which describes the *remote's* reassembly and not anything on the way there.
    Implausible,
}

/// Decides what one remote error means for the client, or refuses it.
pub fn translate(
    ipv6: bool,
    icmp_type: u8,
    code: u8,
    info: u32,
    correlation: Correlation,
) -> Result<Reason, Untranslatable> {
    // Whether one datagram is identified, which is the only proof that carries a claim about one datagram.
    let identified = matches!(correlation, Correlation::Datagram { .. });
    if ipv6 {
        match (icmp_type, code) {
            (ICMPV6_DESTINATION_UNREACHABLE, _) if !identified => Err(Untranslatable::Uncorrelated),
            // Repeated verbatim within the family. RFC 4443 defines codes 0-6; anything beyond them is a code
            // this daemon has no meaning for and will not invent one by passing on.
            (ICMPV6_DESTINATION_UNREACHABLE, 0..=6) => Ok(Reason::Unreachable { code }),
            (ICMPV6_DESTINATION_UNREACHABLE, _) => Err(Untranslatable::Implausible),
            (ICMPV6_PACKET_TOO_BIG, _) if info >= IPV6_MINIMUM_MTU => {
                Ok(Reason::TooBig { mtu: info })
            }
            (ICMPV6_PACKET_TOO_BIG, _) => Err(Untranslatable::Implausible),
            // Code 1 is the remote's own reassembly timer, which says nothing about the path and must not be
            // repeated as though a hop had discarded the packet.
            (ICMPV6_TIME_EXCEEDED, TTL_EXCEEDED_IN_TRANSIT) => Ok(Reason::Expired),
            (ICMPV6_TIME_EXCEEDED, _) => Err(Untranslatable::Implausible),
            _ => Err(Untranslatable::Unsupported),
        }
    } else {
        match (icmp_type, code) {
            (ICMPV4_DESTINATION_UNREACHABLE, ICMPV4_FRAGMENTATION_NEEDED)
                if info >= IPV4_MINIMUM_MTU =>
            {
                Ok(Reason::TooBig { mtu: info })
            }
            // A router that reports Fragmentation Needed without an MTU is following RFC 792 rather than
            // RFC 1191, and there is nothing to pass on: guessing one is exactly what the design forbids.
            (ICMPV4_DESTINATION_UNREACHABLE, ICMPV4_FRAGMENTATION_NEEDED) => {
                Err(Untranslatable::Implausible)
            }
            (ICMPV4_DESTINATION_UNREACHABLE, _) if !identified => Err(Untranslatable::Uncorrelated),
            // RFC 792 and its successors define codes 0-15; a code past them has no meaning to repeat.
            (ICMPV4_DESTINATION_UNREACHABLE, 0..=15) => Ok(Reason::Unreachable { code }),
            (ICMPV4_DESTINATION_UNREACHABLE, _) => Err(Untranslatable::Implausible),
            // Parameter Problem is left out on purpose rather than forgotten: its pointer identifies a byte of
            // a header the daemon rewrote, so repeating it would point the client at the wrong offset. It needs
            // a pointer mapping, not just correlation.
            (ICMPV4_PARAMETER_PROBLEM, _) => Err(Untranslatable::Unsupported),
            (ICMPV4_TIME_EXCEEDED, TTL_EXCEEDED_IN_TRANSIT) => Ok(Reason::Expired),
            (ICMPV4_TIME_EXCEEDED, _) => Err(Untranslatable::Implausible),
            _ => Err(Untranslatable::Unsupported),
        }
    }
}

/// Builds the packet that repeats one remote error to `client`, or says why it cannot be repeated.
pub fn repeat(
    client: SocketAddr,
    error: &Reported,
    correlation: Correlation,
) -> Result<Vec<u8>, Untranslatable> {
    // A UDP error always names its destination, so one that does not is not about a datagram this can describe.
    let Some(destination) = error.destination else {
        return Err(Untranslatable::Implausible);
    };
    // One family throughout, checked rather than assumed: the quote would otherwise describe a header the
    // client's stack will not parse, and the type numbers mean different things per family.
    if client.is_ipv6() != destination.is_ipv6() || client.is_ipv6() != error.remote.is_ipv6() {
        return Err(Untranslatable::Implausible);
    }
    let reason = translate(
        client.is_ipv6(),
        error.icmp_type,
        error.code,
        error.info,
        correlation,
    )?;
    // The quote is a reconstruction, because the datagram itself was not retained. Its addresses, ports and
    // protocol are exact, and those are what a receiver matches an error to a socket on; the hop limit is the
    // error's own rather than the client's original, and nothing matches on that field. The payload is left
    // empty for the same reason - RFC 792 asks for eight bytes of it, and inventing eight would be worse than
    // sending none, since a client that compared them would find them wrong.
    let hop_limit = match correlation {
        Correlation::Datagram { hop_limit } => hop_limit,
        Correlation::Address => error.hop_limit,
    };
    let invoking = build_reply(client, destination, hop_limit, None, &[])
        .map_err(|_| Untranslatable::Implausible)?;
    // Sourced from the router, which is the whole point: an error the daemon originates comes from the
    // interface because the daemon decided, and a repeated one has to come from whoever decided instead.
    icmp_error::build(error.remote, &invoking, reason).map_err(|_| Untranslatable::Implausible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::packet_writer::{validate, IPV4_HEADER_LEN, IPV6_HEADER_LEN};

    fn reported(ipv6: bool, icmp_type: u8, code: u8, info: u32) -> Reported {
        let (remote, destination): (IpAddr, SocketAddr) = if ipv6 {
            (
                "2001:db8:ffff::1".parse().unwrap(),
                "[2606:4700::1111]:443".parse().unwrap(),
            )
        } else {
            (
                "198.51.100.9".parse().unwrap(),
                "93.184.216.34:443".parse().unwrap(),
            )
        };
        Reported {
            remote,
            destination: Some(destination),
            hop_limit: 57,
            icmp_type,
            code,
            info,
            quoted: Quote::new(b"query"),
        }
    }

    fn client(ipv6: bool) -> SocketAddr {
        if ipv6 {
            "[2001:db8:1::2]:40000".parse().unwrap()
        } else {
            "192.0.2.1:40000".parse().unwrap()
        }
    }

    fn octets(address: IpAddr) -> Vec<u8> {
        match address {
            IpAddr::V4(address) => address.octets().to_vec(),
            IpAddr::V6(address) => address.octets().to_vec(),
        }
    }

    #[test]
    fn a_repeated_error_comes_from_the_router_and_quotes_the_client() {
        for (ipv6, icmp_type, code) in [(false, 3u8, 4u8), (true, 2, 0)] {
            let error = reported(ipv6, icmp_type, code, 1400);
            let packet = repeat(client(ipv6), &error, Correlation::Address).unwrap();
            assert_eq!(validate(&packet, 1500), Ok(()), "ipv6 {ipv6}");
            let (source, destination, header) = if ipv6 {
                (&packet[8..24], &packet[24..40], IPV6_HEADER_LEN)
            } else {
                (&packet[12..16], &packet[16..20], IPV4_HEADER_LEN)
            };
            assert_eq!(source, octets(error.remote), "ipv6 {ipv6}");
            assert_eq!(destination, octets(client(ipv6).ip()), "ipv6 {ipv6}");
            let quote = &packet[header + 8..];
            let (quoted_source, quoted_destination) = if ipv6 {
                (&quote[8..24], &quote[24..40])
            } else {
                (&quote[12..16], &quote[16..20])
            };
            assert_eq!(quoted_source, octets(client(ipv6).ip()), "ipv6 {ipv6}");
            assert_eq!(
                quoted_destination,
                octets(error.destination.unwrap().ip()),
                "ipv6 {ipv6}"
            );
        }
    }

    #[test]
    fn the_reported_mtu_reaches_the_wire_in_both_field_layouts() {
        let packet = repeat(
            client(false),
            &reported(false, 3, 4, 1400),
            Correlation::Address,
        )
        .unwrap();
        assert_eq!(
            u16::from_be_bytes([packet[IPV4_HEADER_LEN + 6], packet[IPV4_HEADER_LEN + 7]]),
            1400
        );
        let packet = repeat(
            client(true),
            &reported(true, 2, 0, 1400),
            Correlation::Address,
        )
        .unwrap();
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
    fn a_mismatched_family_is_refused_before_a_quote_is_built() {
        assert_eq!(
            repeat(
                client(false),
                &reported(true, 2, 0, 1400),
                Correlation::Address
            ),
            Err(Untranslatable::Implausible)
        );
        assert_eq!(
            repeat(
                client(true),
                &reported(false, 3, 4, 1400),
                Correlation::Address
            ),
            Err(Untranslatable::Implausible)
        );
    }

    #[test]
    fn a_datagram_claim_needs_datagram_proof() {
        assert_eq!(
            repeat(
                client(false),
                &reported(false, 3, 3, 0),
                Correlation::Address
            ),
            Err(Untranslatable::Uncorrelated)
        );
        assert_eq!(
            repeat(client(true), &reported(true, 1, 3, 0), Correlation::Address),
            Err(Untranslatable::Uncorrelated)
        );
        for (ipv6, icmp_type) in [(false, 3u8), (true, 1u8)] {
            let packet = repeat(
                client(ipv6),
                &reported(ipv6, icmp_type, 3, 0),
                Correlation::Datagram { hop_limit: 64 },
            )
            .unwrap();
            assert_eq!(validate(&packet, 1500), Ok(()), "ipv6 {ipv6}");
            let header = if ipv6 {
                IPV6_HEADER_LEN
            } else {
                IPV4_HEADER_LEN
            };
            assert_eq!(packet[header], icmp_type, "ipv6 {ipv6}");
            assert_eq!(packet[header + 1], 3, "ipv6 {ipv6}");
            let quote = &packet[header + 8..];
            assert_eq!(quote[if ipv6 { 7 } else { 8 }], 64, "ipv6 {ipv6}");
        }
    }

    #[test]
    fn a_code_outside_the_defined_space_is_not_invented() {
        assert_eq!(
            repeat(
                client(false),
                &reported(false, 3, 16, 0),
                Correlation::Datagram { hop_limit: 64 }
            ),
            Err(Untranslatable::Implausible)
        );
        assert_eq!(
            repeat(
                client(true),
                &reported(true, 1, 7, 0),
                Correlation::Datagram { hop_limit: 64 }
            ),
            Err(Untranslatable::Implausible)
        );
    }

    #[test]
    fn a_parameter_problem_is_left_out_even_with_datagram_proof() {
        assert_eq!(
            repeat(
                client(false),
                &reported(false, 12, 0, 0),
                Correlation::Datagram { hop_limit: 64 }
            ),
            Err(Untranslatable::Unsupported)
        );
        assert_eq!(
            repeat(
                client(false),
                &reported(false, 3, 4, 20),
                Correlation::Address
            ),
            Err(Untranslatable::Implausible)
        );
    }

    #[test]
    fn a_too_big_carries_its_mtu_through_in_both_families() {
        assert_eq!(
            translate(
                false,
                ICMPV4_DESTINATION_UNREACHABLE,
                4,
                1400,
                Correlation::Address
            ),
            Ok(Reason::TooBig { mtu: 1400 })
        );
        assert_eq!(
            translate(true, ICMPV6_PACKET_TOO_BIG, 0, 1400, Correlation::Address),
            Ok(Reason::TooBig { mtu: 1400 })
        );
    }

    #[test]
    fn an_mtu_below_the_family_minimum_is_refused() {
        assert_eq!(
            translate(
                false,
                ICMPV4_DESTINATION_UNREACHABLE,
                4,
                0,
                Correlation::Address
            ),
            Err(Untranslatable::Implausible)
        );
        assert_eq!(
            translate(
                false,
                ICMPV4_DESTINATION_UNREACHABLE,
                4,
                67,
                Correlation::Address
            ),
            Err(Untranslatable::Implausible)
        );
        assert_eq!(
            translate(true, ICMPV6_PACKET_TOO_BIG, 0, 1279, Correlation::Address),
            Err(Untranslatable::Implausible)
        );
        assert_eq!(
            translate(
                false,
                ICMPV4_DESTINATION_UNREACHABLE,
                4,
                576,
                Correlation::Address
            ),
            Ok(Reason::TooBig { mtu: 576 })
        );
        assert_eq!(
            translate(true, ICMPV6_PACKET_TOO_BIG, 0, 576, Correlation::Address),
            Err(Untranslatable::Implausible)
        );
    }

    #[test]
    fn a_hop_limit_expiry_translates_but_a_reassembly_timeout_does_not() {
        assert_eq!(
            translate(false, ICMPV4_TIME_EXCEEDED, 0, 0, Correlation::Address),
            Ok(Reason::Expired)
        );
        assert_eq!(
            translate(true, ICMPV6_TIME_EXCEEDED, 0, 0, Correlation::Address),
            Ok(Reason::Expired)
        );
        assert_eq!(
            translate(false, ICMPV4_TIME_EXCEEDED, 1, 0, Correlation::Address),
            Err(Untranslatable::Implausible)
        );
        assert_eq!(
            translate(true, ICMPV6_TIME_EXCEEDED, 1, 0, Correlation::Address),
            Err(Untranslatable::Implausible)
        );
    }

    #[test]
    fn the_families_do_not_share_a_numeric_path() {
        assert_eq!(
            translate(true, 3, TTL_EXCEEDED_IN_TRANSIT, 0, Correlation::Address),
            Ok(Reason::Expired)
        );
        assert_eq!(
            translate(false, 3, TTL_EXCEEDED_IN_TRANSIT, 0, Correlation::Address),
            Err(Untranslatable::Uncorrelated)
        );
        assert_eq!(
            translate(false, ICMPV6_PACKET_TOO_BIG, 0, 1400, Correlation::Address),
            Err(Untranslatable::Unsupported)
        );
    }

    #[test]
    fn address_proof_does_not_carry_a_datagram_claim() {
        for code in [0, 1, 2, 3, 9, 10, 13] {
            assert_eq!(
                translate(
                    false,
                    ICMPV4_DESTINATION_UNREACHABLE,
                    code,
                    0,
                    Correlation::Address
                ),
                Err(Untranslatable::Uncorrelated)
            );
        }
        for code in 0..=6 {
            assert_eq!(
                translate(
                    true,
                    ICMPV6_DESTINATION_UNREACHABLE,
                    code,
                    0,
                    Correlation::Address
                ),
                Err(Untranslatable::Uncorrelated)
            );
        }
        assert_eq!(
            translate(false, 12, 0, 0, Correlation::Datagram { hop_limit: 64 }),
            Err(Untranslatable::Unsupported)
        );
        assert_eq!(
            translate(true, 4, 0, 0, Correlation::Datagram { hop_limit: 64 }),
            Err(Untranslatable::Unsupported)
        );
        assert_eq!(
            translate(false, 0, 0, 0, Correlation::Address),
            Err(Untranslatable::Unsupported)
        );
        assert_eq!(
            translate(true, 129, 0, 0, Correlation::Address),
            Err(Untranslatable::Unsupported)
        );
    }
}
