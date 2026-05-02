use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) struct Route {
    pub(crate) prefix: u128,
    pub(crate) prefix_len: u8,
}

pub(crate) type Network = u64;

#[derive(Clone)]
pub(crate) struct SessionConfig {
    pub(crate) downstream: String,
    pub(crate) dns_bind_address: Ipv4Addr,
    pub(crate) reply_mark: u32,
    pub(crate) primary_network: Option<Network>,
    pub(crate) primary_routes: Vec<Route>,
    pub(crate) fallback_network: Option<Network>,
    pub(crate) ipv6_nat: Option<Ipv6NatConfig>,
}

#[derive(Clone)]
pub(crate) struct Ipv6NatConfig {
    pub(crate) gateway: Ipv6Addr,
    pub(crate) prefix_len: u8,
    pub(crate) mtu: u32,
    pub(crate) suppressed_prefixes: Vec<Route>,
    pub(crate) cleanup_prefixes: Vec<Route>,
}

#[derive(Clone, Copy)]
pub(crate) struct SessionPorts {
    pub(crate) dns_tcp: u16,
    pub(crate) dns_udp: u16,
    pub(crate) ipv6_nat: Option<Ipv6NatPorts>,
}

#[derive(Clone, Copy)]
pub(crate) struct Ipv6NatPorts {
    pub(crate) tcp: u16,
    pub(crate) udp: u16,
}

pub(crate) fn network_prefix(address: Ipv6Addr, prefix_len: u8) -> [u8; 16] {
    let shift = 128u32.saturating_sub(prefix_len as u32);
    (ipv6_to_u128(address) & (!0u128 << shift)).to_be_bytes()
}

pub(crate) fn select_network(config: &SessionConfig, destination: Ipv6Addr) -> Option<Network> {
    let destination = ipv6_to_u128(destination);
    if config.primary_network.is_some() && route_matches(&config.primary_routes, destination) {
        config.primary_network
    } else {
        config.fallback_network
    }
}

pub(crate) fn ipv6_to_u128(address: Ipv6Addr) -> u128 {
    u128::from_be_bytes(address.octets())
}

fn route_matches(routes: &[Route], destination: u128) -> bool {
    routes
        .iter()
        .any(|route| prefix_matches(destination, route.prefix, route.prefix_len))
}

fn prefix_matches(destination: u128, prefix: u128, prefix_len: u8) -> bool {
    if prefix_len == 0 {
        true
    } else {
        let shift = 128 - prefix_len as u32;
        destination >> shift == prefix >> shift
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_route_match_selects_primary_network() {
        let config = config(Some(123), vec![route("2001:db8::", 32)], Some(456));
        assert_eq!(
            select_network(&config, "2001:db8:1::1".parse().unwrap()),
            Some(123)
        );
    }

    #[test]
    fn non_primary_destination_selects_fallback_network() {
        let config = config(Some(123), vec![route("2001:db8::", 32)], Some(456));
        assert_eq!(
            select_network(&config, "fd00::1".parse().unwrap()),
            Some(456)
        );
    }

    #[test]
    fn missing_fallback_network_returns_none() {
        let config = config(Some(123), vec![route("2001:db8::", 32)], None);
        assert_eq!(select_network(&config, "fd00::1".parse().unwrap()), None);
    }

    #[test]
    fn primary_absent_selects_fallback_network() {
        let config = config(None, vec![route("::", 0)], Some(456));
        assert_eq!(
            select_network(&config, "2001:db8::1".parse().unwrap()),
            Some(456)
        );
    }

    fn config(
        primary_network: Option<Network>,
        primary_routes: Vec<Route>,
        fallback_network: Option<Network>,
    ) -> SessionConfig {
        SessionConfig {
            downstream: "wlan0".to_string(),
            dns_bind_address: Ipv4Addr::new(192, 0, 2, 1),
            reply_mark: 0,
            primary_network,
            primary_routes,
            fallback_network,
            ipv6_nat: None,
        }
    }

    fn route(address: &str, prefix_len: u8) -> Route {
        Route {
            prefix: ipv6_to_u128(address.parse::<Ipv6Addr>().unwrap()),
            prefix_len,
        }
    }
}
