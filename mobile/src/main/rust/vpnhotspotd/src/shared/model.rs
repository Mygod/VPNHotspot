use std::net::{Ipv4Addr, Ipv6Addr};

use cidr::{Ipv6Cidr, Ipv6Inet};

pub type Network = u64;

/// Daemon reply sockets use Android's local-network fwmark so AOSP routes them through
/// local_network before VPN UID rules. This is LOCAL_NET_ID 99 plus explicitlySelected and
/// protectedFromVpn.
///
/// Sources:
/// https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/server/binder/android/net/INetd.aidl#768
/// https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/include/Fwmark.h#24
/// https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/server/RouteController.cpp#653
/// https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-15.0.0_r1/service-t/src/com/android/server/NsdService.java#1761
/// https://android.googlesource.com/platform/system/netd/+/android-16.0.0_r1/include/Fwmark.h#24
/// https://android.googlesource.com/platform/system/netd/+/android-16.0.0_r1/server/RouteController.cpp#605
pub const DAEMON_REPLY_MARK: u32 = 0x0003_0063;
pub const DAEMON_REPLY_MARK_MASK: u32 = 0x0003_FFFF;
/// Android fwmark uses the low bits for netId and platform routing metadata. Keep IPv6 NAT
/// TPROXY marks in the high-bit reserved area and always match through the mask.
///
/// Sources:
/// https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/include/Fwmark.h#24
/// https://android.googlesource.com/platform/system/netd/+/e11b8688b1f99292ade06f89f957c1f7e76ceae9/include/Fwmark.h#24
pub const DAEMON_INTERCEPT_FWMARK_VALUE: u32 = 0x1000_0000;
pub const DAEMON_INTERCEPT_FWMARK_MASK: u32 = 0x1000_0000;
/// Android interface route tables start at ifindex + 1000. Use 900 to leave buffer below
/// that range while avoiding kernel-reserved tables and AOSP's fixed 97..99 tables.
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
    pub primary_routes: Vec<Ipv6Cidr>,
    pub fallback_network: Option<Network>,
    pub primary_upstream_interfaces: Vec<String>,
    pub fallback_upstream_interfaces: Vec<String>,
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
    pub gateway: Ipv6Inet,
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

pub fn ipv6_nat_prefix(seed: &str, interface: &str) -> Ipv6Cidr {
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
    Ipv6Cidr::new(Ipv6Addr::from(raw), 64).expect("generated NAT66 prefix is valid")
}

pub fn ipv6_nat_gateway(prefix: Ipv6Cidr) -> Ipv6Inet {
    let mut raw = prefix.first_address().octets();
    raw[15] = 1;
    Ipv6Inet::new(Ipv6Addr::from(raw), prefix.network_length())
        .expect("generated NAT66 gateway is valid")
}

pub fn select_network(config: &SessionConfig, destination: Ipv6Addr) -> Option<Network> {
    select_upstream_network(config, destination).map(|selection| selection.network)
}

pub fn select_upstream_network(
    config: &SessionConfig,
    destination: Ipv6Addr,
) -> Option<SelectedNetwork> {
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

fn route_matches(routes: &[Ipv6Cidr], destination: Ipv6Addr) -> bool {
    routes.iter().any(|route| route.contains(&destination))
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
    fn primary_route_with_host_bits_selects_primary_network() {
        let config = config(Some(123), vec![route("2001:db8::1", 32)], Some(456));
        assert_eq!(
            select_network(&config, "2001:db8:1::1".parse().unwrap()),
            Some(123)
        );
    }

    #[test]
    fn ipv6_nat_prefix_matches_ula_shape() {
        let prefix = ipv6_nat_prefix("be.mygod.vpnhotspot\0android-id", "wlan0");
        assert_eq!(prefix.network_length(), 64);
        assert_eq!(
            prefix.first_address(),
            "fd8d:32f9:31e3:b417::".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(
            ipv6_nat_gateway(prefix).address(),
            "fd8d:32f9:31e3:b417::1".parse::<Ipv6Addr>().unwrap()
        );
    }

    fn config(
        primary_network: Option<Network>,
        primary_routes: Vec<Ipv6Cidr>,
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
            primary_upstream_interfaces: Vec::new(),
            fallback_upstream_interfaces: Vec::new(),
            clients: Vec::new(),
            ipv6_nat: None,
        }
    }

    fn route(address: &str, prefix_len: u8) -> Ipv6Cidr {
        Ipv6Inet::new(address.parse::<Ipv6Addr>().unwrap(), prefix_len)
            .unwrap()
            .network()
    }
}
