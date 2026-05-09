use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Route {
    pub prefix: u128,
    pub prefix_len: u8,
}

pub type Network = u64;

pub const DAEMON_REPLY_MARK: u32 = 0x0003_0063;
pub const DAEMON_REPLY_MARK_MASK: u32 = 0x0003_FFFF;
pub const DAEMON_INTERCEPT_FWMARK_VALUE: u32 = 0x1000_0000;
pub const DAEMON_INTERCEPT_FWMARK_MASK: u32 = 0x1000_0000;
pub const DAEMON_TABLE: u32 = 900;
pub const LOCAL_NETWORK_TABLE: u32 = 99;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionConfig {
    pub downstream: String,
    pub reply_mark: u32,
    pub ip_forward: bool,
    pub masquerade: MasqueradeMode,
    pub ipv6_block: bool,
    pub primary_network: Option<Network>,
    pub primary_routes: Vec<Route>,
    pub fallback_network: Option<Network>,
    pub upstreams: Vec<UpstreamConfig>,
    pub clients: Vec<ClientConfig>,
    pub ipv6_nat: Option<Ipv6NatConfig>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MasqueradeMode {
    None,
    Simple,
    Netd,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UpstreamRole {
    Primary,
    Fallback,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct UpstreamConfig {
    pub ifname: String,
    pub role: UpstreamRole,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectedNetwork {
    pub role: UpstreamRole,
    pub network: Network,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ClientConfig {
    pub mac: [u8; 6],
    pub ipv4: Vec<Ipv4Addr>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ipv6NatConfig {
    pub gateway: Ipv6Addr,
    pub prefix_len: u8,
}

#[derive(Clone, Copy)]
pub struct SessionPorts {
    pub dns_tcp: u16,
    pub dns_udp: u16,
    pub ipv6_nat: Option<Ipv6NatPorts>,
}

#[derive(Clone, Copy)]
pub struct Ipv6NatPorts {
    pub tcp: u16,
    pub udp: u16,
}

pub fn network_prefix(address: Ipv6Addr, prefix_len: u8) -> [u8; 16] {
    let shift = 128u32.saturating_sub(prefix_len as u32);
    (ipv6_to_u128(address) & (!0u128 << shift)).to_be_bytes()
}

pub fn ipv6_nat_prefix(seed: &str, interface: &str) -> Route {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    // This only needs stable local address selection, not cryptographic unpredictability.
    fn update(mut hash: u64, bytes: &[u8]) -> u64 {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    let hash = update(
        update(update(FNV_OFFSET_BASIS, seed.as_bytes()), b"\0"),
        interface.as_bytes(),
    );
    let mut raw = [0u8; 16];
    raw[0] = 0xfd;
    raw[1..8].copy_from_slice(&hash.to_be_bytes()[..7]);
    Route {
        prefix: u128::from_be_bytes(raw),
        prefix_len: 64,
    }
}

pub fn ipv6_nat_gateway(prefix: Route) -> Ipv6Addr {
    let mut raw = prefix.prefix.to_be_bytes();
    raw[15] = 1;
    Ipv6Addr::from(raw)
}

pub fn select_network(config: &SessionConfig, destination: Ipv6Addr) -> Option<Network> {
    select_upstream_network(config, destination).map(|selection| selection.network)
}

pub fn select_upstream_network(
    config: &SessionConfig,
    destination: Ipv6Addr,
) -> Option<SelectedNetwork> {
    let destination = ipv6_to_u128(destination);
    if let Some(network) = config
        .primary_network
        .filter(|_| route_matches(&config.primary_routes, destination))
    {
        Some(SelectedNetwork {
            role: UpstreamRole::Primary,
            network,
        })
    } else {
        config.fallback_network.map(|network| SelectedNetwork {
            role: UpstreamRole::Fallback,
            network,
        })
    }
}

pub fn ipv6_to_u128(address: Ipv6Addr) -> u128 {
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
        assert_eq!(
            select_upstream_network(&config, "2001:db8:1::1".parse().unwrap()),
            Some(SelectedNetwork {
                role: UpstreamRole::Primary,
                network: 123
            })
        );
    }

    #[test]
    fn non_primary_destination_selects_fallback_network() {
        let config = config(Some(123), vec![route("2001:db8::", 32)], Some(456));
        assert_eq!(
            select_network(&config, "fd00::1".parse().unwrap()),
            Some(456)
        );
        assert_eq!(
            select_upstream_network(&config, "fd00::1".parse().unwrap()),
            Some(SelectedNetwork {
                role: UpstreamRole::Fallback,
                network: 456
            })
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

    #[test]
    fn ipv6_nat_prefix_matches_ula_shape() {
        let prefix = ipv6_nat_prefix("be.mygod.vpnhotspot\0android-id", "wlan0");
        assert_eq!(prefix.prefix_len, 64);
        assert_eq!(
            Ipv6Addr::from(prefix.prefix.to_be_bytes()),
            "fd8d:32f9:31e3:b417::".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(
            ipv6_nat_gateway(prefix),
            "fd8d:32f9:31e3:b417::1".parse::<Ipv6Addr>().unwrap()
        );
    }

    fn config(
        primary_network: Option<Network>,
        primary_routes: Vec<Route>,
        fallback_network: Option<Network>,
    ) -> SessionConfig {
        SessionConfig {
            downstream: "wlan0".to_string(),
            reply_mark: 0,
            ip_forward: false,
            masquerade: MasqueradeMode::None,
            ipv6_block: false,
            primary_network,
            primary_routes,
            fallback_network,
            upstreams: Vec::new(),
            clients: Vec::new(),
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
