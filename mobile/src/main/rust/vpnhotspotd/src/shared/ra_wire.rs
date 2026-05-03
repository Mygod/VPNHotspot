use std::net::Ipv6Addr;

use crate::shared::model::{network_prefix, Route};

pub fn make_current_ra_packet(gateway: Ipv6Addr, prefix_len: u8, mtu: u32) -> Vec<u8> {
    RaAdvertisement {
        dns_server: gateway,
        advertised_prefix: gateway,
        prefix_len,
        mtu,
        router_lifetime: 1800,
        valid_lifetime: 3600,
        preferred_lifetime: 1800,
        rdnss_lifetime: 600,
    }
    .encode()
}

pub fn make_zero_lifetime_ra_packet(prefix: Route, mtu: u32, keep_router: bool) -> Vec<u8> {
    let gateway = Ipv6Addr::from(prefix.prefix);
    RaAdvertisement {
        dns_server: gateway,
        advertised_prefix: gateway,
        prefix_len: prefix.prefix_len,
        mtu,
        router_lifetime: if keep_router { 1800 } else { 0 },
        valid_lifetime: 0,
        preferred_lifetime: 0,
        rdnss_lifetime: 0,
    }
    .encode()
}

pub fn is_router_link_local(address: Ipv6Addr) -> bool {
    address.is_unicast_link_local() && !address.is_loopback() && !address.is_multicast()
}

struct RaAdvertisement {
    dns_server: Ipv6Addr,
    advertised_prefix: Ipv6Addr,
    prefix_len: u8,
    mtu: u32,
    router_lifetime: u16,
    valid_lifetime: u32,
    preferred_lifetime: u32,
    rdnss_lifetime: u32,
}

impl RaAdvertisement {
    fn encode(self) -> Vec<u8> {
        let mut packet = Vec::with_capacity(80);
        packet.push(134);
        packet.push(0);
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.push(64);
        packet.push(0);
        packet.extend_from_slice(&self.router_lifetime.to_be_bytes());
        packet.extend_from_slice(&0u32.to_be_bytes());
        packet.extend_from_slice(&0u32.to_be_bytes());

        packet.push(3);
        packet.push(4);
        packet.push(self.prefix_len);
        packet.push(0xc0);
        packet.extend_from_slice(&self.valid_lifetime.to_be_bytes());
        packet.extend_from_slice(&self.preferred_lifetime.to_be_bytes());
        packet.extend_from_slice(&0u32.to_be_bytes());
        packet.extend_from_slice(&network_prefix(self.advertised_prefix, self.prefix_len));

        packet.push(5);
        packet.push(1);
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(&self.mtu.to_be_bytes());

        packet.push(25);
        packet.push(3);
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(&self.rdnss_lifetime.to_be_bytes());
        packet.extend_from_slice(&self.dns_server.octets());
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_lifetime_ra_withdraws_dns_server() {
        let dns_server: Ipv6Addr = "fd47:6b7c:2186:b452::1".parse().unwrap();
        let packet = RaAdvertisement {
            dns_server,
            advertised_prefix: dns_server,
            prefix_len: 64,
            mtu: 1500,
            router_lifetime: 0,
            valid_lifetime: 0,
            preferred_lifetime: 0,
            rdnss_lifetime: 0,
        }
        .encode();
        assert_eq!(&packet[40..44], &0u32.to_be_bytes());
        assert_eq!(&packet[packet.len() - 16..], &dns_server.octets());
    }

    #[test]
    fn router_link_local_filter_accepts_only_unicast_link_local() {
        assert!(is_router_link_local("fe80::1".parse().unwrap()));
        assert!(!is_router_link_local("fd00::1".parse().unwrap()));
        assert!(!is_router_link_local("ff02::1".parse().unwrap()));
        assert!(!is_router_link_local("::1".parse().unwrap()));
    }
}
