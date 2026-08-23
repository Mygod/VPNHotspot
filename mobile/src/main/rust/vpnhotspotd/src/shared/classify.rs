//! Classification of packets read from the TUN.
//!
//! Every packet is untrusted input from an unknown local principal: the restricted network denies
//! `Network`-handle selection but not interface selection, so anything on this interface may have been
//! injected by any app on the device. Nothing here derives identity from a source address.
//!
//! Destination is compared against the session's exact virtual-address set *before* attribution,
//! reassembly, or transport dispatch, because those addresses are the ones the daemon answers for rather
//! than relays.

use std::net::IpAddr;

/// The three shared principals. None of them is a client identity: per-client principals were the original
/// design and are not achievable, since nothing distinguishes a tethered client's source address from one
/// a local app chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Principal {
    /// Traffic to a configured virtual-DNS endpoint, which the daemon terminates.
    Dns,
    Ipv4,
    Ipv6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drop {
    /// Not a well-formed packet of the family it claims, or truncated.
    Malformed,
    /// Some other protocol or port aimed at an address the daemon occupies. Dropped without a response
    /// and without an upstream socket, because the daemon is the only thing at that address and it does
    /// not offer that service.
    Reserved,
    /// A destination that means something only on the link it arrived from: multicast, broadcast, link
    /// local, loopback, or unspecified. Re-originating one onto the upstream would put it on a foreign
    /// link, which both leaks what the sender meant to keep local - mDNS service discovery, most of it -
    /// and asks the upstream to answer for a scope it is not in.
    ///
    /// Private and unique-local addresses are deliberately not here. They are ordinary destinations for a
    /// VPN or a NATted upstream, and the resolver this daemon relays to is usually one of them.
    Unroutable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Classified {
    Accepted {
        principal: Principal,
        /// A fragment for a virtual address is a provisional [Principal::Dns] candidate until its
        /// transport header is available, so reassembly is charged to the principal it will belong to
        /// rather than to whoever happens to complete it.
        provisional: bool,
    },
    Dropped(Drop),
}

const PROTOCOL_TCP: u8 = 6;
const PROTOCOL_UDP: u8 = 17;
const IPV6_FRAGMENT: u8 = 44;
const DNS_PORT: u16 = 53;
const IPV4_MIN_HEADER: usize = 20;
const IPV6_HEADER: usize = 40;

/// What the header said, as far as classification needs it.
struct Parsed {
    destination: IpAddr,
    protocol: u8,
    /// None when the transport header is not in this packet, which is the non-first-fragment case.
    destination_port: Option<u16>,
    fragment: bool,
}

pub fn classify(packet: &[u8], virtual_addresses: &[IpAddr]) -> Classified {
    let Some(parsed) = parse(packet) else {
        return Classified::Dropped(Drop::Malformed);
    };
    if virtual_addresses.contains(&parsed.destination) {
        // the transport header is missing precisely when this is a later fragment, and a later fragment of
        // a datagram aimed at a virtual address can only belong to that address
        if parsed.fragment && parsed.destination_port.is_none() {
            return Classified::Accepted {
                principal: Principal::Dns,
                provisional: true,
            };
        }
        let dns = matches!(parsed.protocol, PROTOCOL_TCP | PROTOCOL_UDP)
            && parsed.destination_port == Some(DNS_PORT);
        return if dns {
            Classified::Accepted {
                principal: Principal::Dns,
                provisional: false,
            }
        } else {
            Classified::Dropped(Drop::Reserved)
        };
    }
    // after the virtual set, because those addresses are the daemon's own and are answered rather than
    // forwarded, and before attribution, because an unroutable destination is never relayed at all
    match parsed.destination {
        IpAddr::V4(destination) => {
            if destination.is_multicast()
                || destination.is_broadcast()
                || destination.is_link_local()
                || destination.is_loopback()
                || destination.is_unspecified()
            {
                return Classified::Dropped(Drop::Unroutable);
            }
            Classified::Accepted {
                principal: Principal::Ipv4,
                provisional: false,
            }
        }
        IpAddr::V6(destination) => {
            if destination.is_multicast()
                || destination.is_unicast_link_local()
                || destination.is_loopback()
                || destination.is_unspecified()
            {
                return Classified::Dropped(Drop::Unroutable);
            }
            Classified::Accepted {
                principal: Principal::Ipv6,
                provisional: false,
            }
        }
    }
}

fn parse(packet: &[u8]) -> Option<Parsed> {
    match packet.first()? >> 4 {
        4 => {
            let header_length = ((packet[0] & 0xf) as usize) * 4;
            if header_length < IPV4_MIN_HEADER || packet.len() < header_length {
                return None;
            }
            if u16::from_be_bytes([packet[2], packet[3]]) as usize != packet.len() {
                return None;
            }
            let fragment_field = u16::from_be_bytes([packet[6], packet[7]]);
            // more-fragments flag, or a non-zero offset
            let fragment = fragment_field & 0x2000 != 0 || fragment_field & 0x1fff != 0;
            let first = fragment_field & 0x1fff == 0;
            let protocol = packet[9];
            Some(Parsed {
                destination: IpAddr::from(<[u8; 4]>::try_from(&packet[16..20]).ok()?),
                protocol,
                destination_port: if first {
                    destination_port(&packet[header_length..], protocol)
                } else {
                    None
                },
                fragment,
            })
        }
        6 => {
            if packet.len() < IPV6_HEADER {
                return None;
            }
            if u16::from_be_bytes([packet[4], packet[5]]) as usize != packet.len() - IPV6_HEADER {
                return None;
            }
            let destination = IpAddr::from(<[u8; 16]>::try_from(&packet[24..40]).ok()?);
            let next_header = packet[6];
            if next_header != IPV6_FRAGMENT {
                return Some(Parsed {
                    destination,
                    protocol: next_header,
                    destination_port: destination_port(&packet[IPV6_HEADER..], next_header),
                    fragment: false,
                });
            }
            // one fragment header is parsed here because it decides whether the transport header is
            // present at all; the bounded walk over the rest of the chain belongs with reassembly
            let fragment = packet.get(IPV6_HEADER..IPV6_HEADER + 8)?;
            let protocol = fragment[0];
            let offset = u16::from_be_bytes([fragment[2], fragment[3]]) & !7;
            Some(Parsed {
                destination,
                protocol,
                destination_port: if offset == 0 {
                    destination_port(&packet[IPV6_HEADER + 8..], protocol)
                } else {
                    None
                },
                fragment: true,
            })
        }
        _ => None,
    }
}

fn destination_port(transport: &[u8], protocol: u8) -> Option<u16> {
    if !matches!(protocol, PROTOCOL_TCP | PROTOCOL_UDP) || transport.len() < 4 {
        return None;
    }
    Some(u16::from_be_bytes([transport[2], transport[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const DNS4: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 5));
    const DNS6: IpAddr = IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x53));

    fn virtual_addresses() -> Vec<IpAddr> {
        vec![DNS4, DNS6]
    }

    /// IPv4 with a UDP or TCP header, optionally a later fragment.
    fn ipv4(destination: Ipv4Addr, protocol: u8, port: u16, fragment_field: u16) -> Vec<u8> {
        let mut packet = vec![0x45, 0, 0, 0, 0, 0, 0, 0, 64, protocol, 0, 0];
        packet.extend_from_slice(&Ipv4Addr::new(192, 0, 2, 2).octets());
        packet.extend_from_slice(&destination.octets());
        packet.extend_from_slice(&[0, 0]);
        packet.extend_from_slice(&port.to_be_bytes());
        packet.extend_from_slice(&[0, 0, 0, 0]);
        let length = packet.len() as u16;
        packet[2..4].copy_from_slice(&length.to_be_bytes());
        packet[6..8].copy_from_slice(&fragment_field.to_be_bytes());
        packet
    }

    fn ipv6(destination: Ipv6Addr, next_header: u8, port: u16) -> Vec<u8> {
        let mut packet = vec![0x60, 0, 0, 0, 0, 0, next_header, 64];
        packet.extend_from_slice(&Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 2).octets());
        packet.extend_from_slice(&destination.octets());
        packet.extend_from_slice(&[0, 0]);
        packet.extend_from_slice(&port.to_be_bytes());
        packet.extend_from_slice(&[0, 0, 0, 0]);
        let payload = (packet.len() - IPV6_HEADER) as u16;
        packet[4..6].copy_from_slice(&payload.to_be_bytes());
        packet
    }

    #[test]
    fn exact_virtual_dns_endpoint_is_dns() {
        for protocol in [PROTOCOL_UDP, PROTOCOL_TCP] {
            assert_eq!(
                classify(
                    &ipv4(Ipv4Addr::new(192, 0, 2, 5), protocol, DNS_PORT, 0),
                    &virtual_addresses()
                ),
                Classified::Accepted {
                    principal: Principal::Dns,
                    provisional: false
                }
            );
        }
        assert_eq!(
            classify(
                &ipv6(
                    Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x53),
                    PROTOCOL_UDP,
                    DNS_PORT
                ),
                &virtual_addresses()
            ),
            Classified::Accepted {
                principal: Principal::Dns,
                provisional: false
            }
        );
    }

    #[test]
    fn other_ports_and_protocols_to_a_virtual_address_are_dropped() {
        assert_eq!(
            classify(
                &ipv4(Ipv4Addr::new(192, 0, 2, 5), PROTOCOL_UDP, 443, 0),
                &virtual_addresses()
            ),
            Classified::Dropped(Drop::Reserved)
        );
        // ICMP to a virtual address has no port at all, so it can never be the DNS endpoint
        assert_eq!(
            classify(
                &ipv4(Ipv4Addr::new(192, 0, 2, 5), 1, 0, 0),
                &virtual_addresses()
            ),
            Classified::Dropped(Drop::Reserved)
        );
    }

    #[test]
    fn ordinary_traffic_is_attributed_by_family() {
        assert_eq!(
            classify(
                &ipv4(Ipv4Addr::new(8, 8, 8, 8), PROTOCOL_UDP, DNS_PORT, 0),
                &virtual_addresses()
            ),
            Classified::Accepted {
                principal: Principal::Ipv4,
                provisional: false
            }
        );
        assert_eq!(
            classify(
                &ipv6(
                    Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111),
                    PROTOCOL_UDP,
                    443
                ),
                &virtual_addresses()
            ),
            Classified::Accepted {
                principal: Principal::Ipv6,
                provisional: false
            }
        );
    }

    #[test]
    fn a_later_fragment_for_a_virtual_address_is_provisional() {
        // offset 2 in 8-byte units, no transport header in this packet
        assert_eq!(
            classify(
                &ipv4(Ipv4Addr::new(192, 0, 2, 5), PROTOCOL_UDP, 0, 2),
                &virtual_addresses()
            ),
            Classified::Accepted {
                principal: Principal::Dns,
                provisional: true
            }
        );
        // a first fragment still carries the header, so it is classified exactly
        assert_eq!(
            classify(
                &ipv4(Ipv4Addr::new(192, 0, 2, 5), PROTOCOL_UDP, DNS_PORT, 0x2000),
                &virtual_addresses()
            ),
            Classified::Accepted {
                principal: Principal::Dns,
                provisional: false
            }
        );
    }

    #[test]
    fn malformed_packets_are_dropped() {
        assert_eq!(
            classify(&[], &virtual_addresses()),
            Classified::Dropped(Drop::Malformed)
        );
        assert_eq!(
            classify(&[0x45; 8], &virtual_addresses()),
            Classified::Dropped(Drop::Malformed)
        );
        // total length disagreeing with the actual length
        let mut packet = ipv4(Ipv4Addr::new(8, 8, 8, 8), PROTOCOL_UDP, 53, 0);
        packet.pop();
        assert_eq!(
            classify(&packet, &virtual_addresses()),
            Classified::Dropped(Drop::Malformed)
        );
        // an IPv4 header length below the minimum
        let mut short = ipv4(Ipv4Addr::new(8, 8, 8, 8), PROTOCOL_UDP, 53, 0);
        short[0] = 0x44;
        assert_eq!(
            classify(&short, &virtual_addresses()),
            Classified::Dropped(Drop::Malformed)
        );
    }

    #[test]
    fn link_scoped_destinations_are_never_relayed() {
        for destination in [
            Ipv4Addr::new(224, 0, 0, 251),
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(169, 254, 1, 1),
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::UNSPECIFIED,
        ] {
            assert_eq!(
                classify(
                    &ipv4(destination, PROTOCOL_UDP, 5353, 0),
                    &virtual_addresses()
                ),
                Classified::Dropped(Drop::Unroutable),
                "{destination}"
            );
        }
        for destination in [
            Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb),
            Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
            Ipv6Addr::LOCALHOST,
            Ipv6Addr::UNSPECIFIED,
        ] {
            assert_eq!(
                classify(&ipv6(destination, PROTOCOL_UDP, 5353), &virtual_addresses()),
                Classified::Dropped(Drop::Unroutable),
                "{destination}"
            );
        }
        // a unique-local or private destination is an ordinary one: it is what a VPN's own resolver looks
        // like, so dropping it would break the case this mode exists for
        assert_eq!(
            classify(
                &ipv4(Ipv4Addr::new(10, 128, 0, 1), PROTOCOL_UDP, DNS_PORT, 0),
                &virtual_addresses()
            ),
            Classified::Accepted {
                principal: Principal::Ipv4,
                provisional: false
            }
        );
        assert_eq!(
            classify(
                &ipv6(
                    Ipv6Addr::new(0xfd7d, 0x76ee, 0xe68f, 0xa993, 0, 0, 0, 1),
                    PROTOCOL_UDP,
                    DNS_PORT
                ),
                &virtual_addresses()
            ),
            Classified::Accepted {
                principal: Principal::Ipv6,
                provisional: false
            }
        );
    }

    #[test]
    fn an_empty_virtual_set_classifies_everything_by_family() {
        assert_eq!(
            classify(
                &ipv4(Ipv4Addr::new(192, 0, 2, 5), PROTOCOL_UDP, DNS_PORT, 0),
                &[]
            ),
            Classified::Accepted {
                principal: Principal::Ipv4,
                provisional: false
            }
        );
    }
}
