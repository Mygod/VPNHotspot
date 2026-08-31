use std::net::IpAddr;

#[cfg(test)]
use etherparse::Ipv6Header;
use etherparse::{IpFragOffset, IpNumber, Ipv6FragmentHeaderSlice};

use crate::shared::ip_wire::Packet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Principal {
    Dns,
    Ipv4,
    Ipv6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drop {
    Malformed,
    Reserved,
    Unroutable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Classified {
    Accepted {
        principal: Principal,
        provisional: bool,
    },
    Dropped(Drop),
}

const PROTOCOL_TCP: u8 = 6;
const PROTOCOL_UDP: u8 = 17;
const DNS_PORT: u16 = 53;

struct Parsed {
    destination: IpAddr,
    protocol: u8,
    destination_port: Option<u16>,
    /// The IPv6 base header points directly to an extension chain.
    extended: bool,
    fragment: bool,
}

pub fn classify(packet: &[u8], virtual_addresses: &[IpAddr]) -> Classified {
    let Some(parsed) = parse(packet) else {
        return Classified::Dropped(Drop::Malformed);
    };
    if virtual_addresses.contains(&parsed.destination) {
        // Admit hidden transports only for bounded inspection; the result is classified again.
        if parsed.extended || (parsed.fragment && parsed.destination_port.is_none()) {
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
    match Packet::parse(packet).ok()? {
        Packet::Ipv4 { header, payload } => {
            let fragment = header.is_fragmenting_payload();
            let first = header.fragments_offset() == IpFragOffset::ZERO;
            let protocol = header.protocol().0;
            Some(Parsed {
                destination: IpAddr::V4(header.destination_addr()),
                protocol,
                destination_port: if first {
                    destination_port(payload, protocol)
                } else {
                    None
                },
                extended: false,
                fragment,
            })
        }
        Packet::Ipv6 { header, payload } => {
            let destination = IpAddr::V6(header.destination_addr());
            if header.next_header() != IpNumber::IPV6_FRAGMENTATION_HEADER {
                return Some(Parsed {
                    destination,
                    protocol: header.next_header().0,
                    destination_port: destination_port(payload, header.next_header().0),
                    extended: header.next_header().is_ipv6_ext_header_value(),
                    fragment: false,
                });
            }
            let fragment = Ipv6FragmentHeaderSlice::from_slice(payload).ok()?;
            let protocol = fragment.next_header().0;
            Some(Parsed {
                destination,
                protocol,
                extended: false,
                destination_port: if fragment.fragment_offset() == IpFragOffset::ZERO {
                    destination_port(&payload[fragment.slice().len()..], protocol)
                } else {
                    None
                },
                fragment: true,
            })
        }
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
    use crate::shared::extension;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const DNS4: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 5));
    const DNS6: IpAddr = IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x53));

    fn virtual_addresses() -> Vec<IpAddr> {
        vec![DNS4, DNS6]
    }

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
        let payload = (packet.len() - Ipv6Header::LEN) as u16;
        packet[4..6].copy_from_slice(&payload.to_be_bytes());
        packet
    }

    const HOP_BY_HOP: u8 = 0;
    const ROUTING: u8 = 43;
    const DESTINATION: u8 = 60;
    const AUTHENTICATION: u8 = 51;

    /// Wraps an IPv6 packet in `(kind, length units, third byte)` headers; Routing uses the third byte for
    /// Segments Left.
    fn wrapped(
        destination: Ipv6Addr,
        chain: &[(u8, usize, u8)],
        next_header: u8,
        port: u16,
    ) -> Vec<u8> {
        let inner = ipv6(destination, next_header, port);
        let mut headers = Vec::new();
        for (index, (_, units, third)) in chain.iter().enumerate() {
            let next = chain
                .get(index + 1)
                .map_or(next_header, |(kind, _, _)| *kind);
            let mut header = vec![0u8; (units + 1) * 8];
            header[0] = next;
            header[1] = *units as u8;
            header[3] = *third;
            headers.extend_from_slice(&header);
        }
        let mut packet = inner[..Ipv6Header::LEN].to_vec();
        packet[6] = chain[0].0;
        let payload = headers.len() + inner.len() - Ipv6Header::LEN;
        packet[4..6].copy_from_slice(&(payload as u16).to_be_bytes());
        packet.extend_from_slice(&headers);
        packet.extend_from_slice(&inner[Ipv6Header::LEN..]);
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
    fn an_extension_chain_to_a_virtual_address_is_inspected_and_then_reclassified() {
        for chain in [
            vec![(DESTINATION, 0, 0)],
            vec![(HOP_BY_HOP, 0, 0)],
            vec![(HOP_BY_HOP, 0, 0), (ROUTING, 0, 0), (DESTINATION, 1, 0)],
            std::iter::repeat_n((DESTINATION, 0, 0), 128).collect(),
        ] {
            for protocol in [PROTOCOL_UDP, PROTOCOL_TCP] {
                let packet = wrapped(
                    Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x53),
                    &chain,
                    protocol,
                    DNS_PORT,
                );
                assert_eq!(
                    classify(&packet, &virtual_addresses()),
                    Classified::Accepted {
                        principal: Principal::Dns,
                        provisional: true
                    },
                    "{chain:?} {protocol}"
                );
                let Ok(extension::Normalized {
                    walked: extension::Walked::Stripped(stripped),
                    ..
                }) = extension::walk(&packet)
                else {
                    panic!("{chain:?} should strip");
                };
                assert_eq!(
                    classify(&stripped, &virtual_addresses()),
                    Classified::Accepted {
                        principal: Principal::Dns,
                        provisional: false
                    },
                    "{chain:?} {protocol}"
                );
            }
        }
    }

    #[test]
    fn a_chain_is_no_route_to_a_virtual_address_for_anything_but_the_dns_port() {
        for (protocol, port) in [(PROTOCOL_UDP, 443), (PROTOCOL_TCP, 443), (PROTOCOL_UDP, 0)] {
            let packet = wrapped(
                Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x53),
                &[(DESTINATION, 0, 0)],
                protocol,
                port,
            );
            assert_eq!(
                classify(&packet, &virtual_addresses()),
                Classified::Accepted {
                    principal: Principal::Dns,
                    provisional: true
                },
                "{protocol} {port}"
            );
            let Ok(extension::Normalized {
                walked: extension::Walked::Stripped(stripped),
                ..
            }) = extension::walk(&packet)
            else {
                panic!("{protocol} {port} should strip");
            };
            assert_eq!(
                classify(&stripped, &virtual_addresses()),
                Classified::Dropped(Drop::Reserved),
                "{protocol} {port}"
            );
        }
    }

    #[test]
    fn inspecting_a_chain_to_a_virtual_address_does_not_relax_what_the_walk_refuses() {
        for chain in [vec![(ROUTING, 0, 1)], vec![(AUTHENTICATION, 0, 0)]] {
            let packet = wrapped(
                Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x53),
                &chain,
                PROTOCOL_UDP,
                DNS_PORT,
            );
            assert_eq!(
                classify(&packet, &virtual_addresses()),
                Classified::Accepted {
                    principal: Principal::Dns,
                    provisional: true
                },
                "{chain:?}"
            );
            assert!(extension::walk(&packet).is_err(), "{chain:?}");
        }
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
        let mut packet = ipv4(Ipv4Addr::new(8, 8, 8, 8), PROTOCOL_UDP, 53, 0);
        packet.pop();
        assert_eq!(
            classify(&packet, &virtual_addresses()),
            Classified::Dropped(Drop::Malformed)
        );
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
