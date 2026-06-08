use std::net::{Ipv4Addr, Ipv6Addr};

use cidr::{Ipv6Cidr, Ipv6Inet};

use crate::shared::proto::daemon::MasqueradeMode;

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
/// Android fwmark uses the low bits for netId and platform routing metadata. When IPv6 NAT
/// falls back to fwmark-based TPROXY routing, keep marks in the high-bit reserved area and
/// always match through the mask.
///
/// Sources:
/// https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/include/Fwmark.h#24
/// https://android.googlesource.com/platform/system/netd/+/e11b8688b1f99292ade06f89f957c1f7e76ceae9/include/Fwmark.h#24
pub const DAEMON_INTERCEPT_FWMARK_VALUE: u32 = 0x1000_0000;
pub const DAEMON_INTERCEPT_FWMARK_MASK: u32 = 0x1000_0000;
/// Internal TCP/UDP TPROXY listener address. Intercepted packets still carry their original
/// destination through TCP transparent socket state or IPV6_RECVORIGDSTADDR.
pub const DAEMON_TPROXY_ADDRESS: Ipv6Addr = Ipv6Addr::LOCALHOST;
/// Internal NFQUEUE number for NAT66 ICMPv6 Echo interception.
pub const DAEMON_ICMP_NFQUEUE_NUM: u16 = 30_000;
/// Android interface route tables start at ifindex + 1000. Use 900 to leave buffer below
/// that range while avoiding kernel-reserved tables and AOSP's fixed 97..99 tables.
pub const DAEMON_TABLE: u32 = 900;
/// Android's fixed local_network route table.
///
/// Sources:
/// https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/server/RouteController.cpp#73
/// https://android.googlesource.com/platform/system/netd/+/e11b8688b1f99292ade06f89f957c1f7e76ceae9/server/RouteController.cpp#54
pub const ANDROID_ROUTE_TABLE_LOCAL_NETWORK: u32 = 97;
/// Kernel's main routing table (RT_TABLE_MAIN). Holds the kernel-installed connected route for
/// every interface's own subnet, so a gateway downstream's client subnet is reachable here.
pub const MAIN_TABLE: u32 = 254;

struct KernelRelease {
    major: u32,
    minor: u32,
    patch: Option<u32>,
}

fn parse_kernel_release(release: &str) -> Option<KernelRelease> {
    let (major, rest) = release.split_once('.')?;
    let minor_end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    if minor_end == 0 {
        return None;
    }
    let patch = if rest[minor_end..].starts_with('.') {
        let patch_start = minor_end + 1;
        let patch_end = rest[patch_start..]
            .find(|character: char| !character.is_ascii_digit())
            .map_or(rest.len(), |end| patch_start + end);
        if patch_start == patch_end {
            return None;
        } else {
            Some(rest[patch_start..patch_end].parse::<u32>().ok()?)
        }
    } else {
        None
    };
    Some(KernelRelease {
        major: major.parse::<u32>().ok()?,
        minor: rest[..minor_end].parse::<u32>().ok()?,
        patch,
    })
}

pub fn kernel_release_supports_fra_ip_proto(release: &str) -> Option<bool> {
    let release = parse_kernel_release(release)?;
    Some(release.major > 4 || release.major == 4 && release.minor >= 17)
}

pub fn kernel_release_raw_ipv6_bind_rejection_is_unexpected(release: &str) -> Option<bool> {
    let release = parse_kernel_release(release)?;
    Some(match (release.major, release.minor) {
        (major, _) if major > 5 => true,
        (5, minor) if minor > 11 => true,
        (5, 11) => release.patch.is_some_and(|patch| patch >= 14),
        _ => false,
    })
}

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
    /// Single-arm router downstream: add a return-path rule so VPN replies reach the client subnet.
    pub gateway: bool,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RaPreference {
    High,
    Medium,
    Low,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ipv6NatConfig {
    pub gateway: Ipv6Inet,
    pub ra_preference: RaPreference,
}

#[derive(Clone)]
pub struct SessionPorts {
    pub dns: Vec<ClientDnsPorts>,
    pub ipv6_nat: Option<Ipv6NatPorts>,
}

#[derive(Clone, Copy)]
pub struct ClientDnsPorts {
    pub mac: [u8; 6],
    pub tcp: Option<u16>,
    pub udp: Option<u16>,
}

#[derive(Clone)]
pub struct Ipv6NatPorts {
    pub clients: Vec<ClientIpv6NatPorts>,
    pub icmp_echo: bool,
}

#[derive(Clone, Copy)]
pub struct ClientIpv6NatPorts {
    pub mac: [u8; 6],
    pub tcp: Option<u16>,
    pub udp: Option<u16>,
}

pub fn mac_string(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
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

pub fn has_client_scoped_ipv6_nat_demand(config: &SessionConfig) -> bool {
    config.ipv6_nat.is_some() && !config.clients.is_empty()
}

pub fn should_disable_uncommitted_ipv6_nat(
    config: &SessionConfig,
    committed_ports: &SessionPorts,
) -> bool {
    has_client_scoped_ipv6_nat_demand(config) && committed_ports.ipv6_nat.is_none()
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

    #[test]
    fn kernel_release_supports_fra_ip_proto_from_4_17() {
        assert_eq!(
            kernel_release_supports_fra_ip_proto("4.16.18-android"),
            Some(false)
        );
        assert_eq!(
            kernel_release_supports_fra_ip_proto("4.17.0-g123"),
            Some(true)
        );
        assert_eq!(
            kernel_release_supports_fra_ip_proto("6.1.145-android14-11-gfa1d6308d1fe"),
            Some(true)
        );
    }

    #[test]
    fn kernel_release_supports_fra_ip_proto_returns_none_for_unparsable_release() {
        assert_eq!(kernel_release_supports_fra_ip_proto("android"), None);
        assert_eq!(kernel_release_supports_fra_ip_proto("4.x"), None);
    }

    #[test]
    fn kernel_release_raw_ipv6_bind_rejection_is_unexpected_from_5_11_14() {
        assert_eq!(
            kernel_release_raw_ipv6_bind_rejection_is_unexpected("5.11.13-android"),
            Some(false)
        );
        assert_eq!(
            kernel_release_raw_ipv6_bind_rejection_is_unexpected("5.11.14-g123"),
            Some(true)
        );
        assert_eq!(
            kernel_release_raw_ipv6_bind_rejection_is_unexpected(
                "6.1.145-android14-11-gfa1d6308d1fe"
            ),
            Some(true)
        );
    }

    #[test]
    fn kernel_release_raw_ipv6_bind_rejection_is_expected_on_older_stable_branches() {
        assert_eq!(
            kernel_release_raw_ipv6_bind_rejection_is_unexpected("4.19.187-android"),
            Some(false)
        );
        assert_eq!(
            kernel_release_raw_ipv6_bind_rejection_is_unexpected("5.4.112-android"),
            Some(false)
        );
        assert_eq!(
            kernel_release_raw_ipv6_bind_rejection_is_unexpected("5.10.30-android"),
            Some(false)
        );
    }

    #[test]
    fn kernel_release_raw_ipv6_bind_rejection_returns_none_for_unparsable_release() {
        assert_eq!(
            kernel_release_raw_ipv6_bind_rejection_is_unexpected("android"),
            None
        );
        assert_eq!(
            kernel_release_raw_ipv6_bind_rejection_is_unexpected("5.11.x"),
            None
        );
    }

    #[test]
    fn uncommitted_ipv6_nat_is_deferred_without_clients() {
        let mut config = config(None, Vec::new(), None);
        config.ipv6_nat = Some(ipv6_nat_config());
        assert!(!should_disable_uncommitted_ipv6_nat(
            &config,
            &SessionPorts {
                dns: Vec::new(),
                ipv6_nat: None,
            },
        ));
    }

    #[test]
    fn uncommitted_ipv6_nat_is_disabled_with_clients() {
        let mut config = config(None, Vec::new(), None);
        config.clients.push(ClientConfig {
            mac: [2, 3, 5, 7, 11, 13],
            ipv4: Vec::new(),
        });
        config.ipv6_nat = Some(ipv6_nat_config());
        assert!(should_disable_uncommitted_ipv6_nat(
            &config,
            &SessionPorts {
                dns: Vec::new(),
                ipv6_nat: None,
            },
        ));
    }

    #[test]
    fn committed_ipv6_nat_is_kept_with_clients() {
        let mut config = config(None, Vec::new(), None);
        config.clients.push(ClientConfig {
            mac: [2, 3, 5, 7, 11, 13],
            ipv4: Vec::new(),
        });
        config.ipv6_nat = Some(ipv6_nat_config());
        assert!(!should_disable_uncommitted_ipv6_nat(
            &config,
            &SessionPorts {
                dns: Vec::new(),
                ipv6_nat: Some(Ipv6NatPorts {
                    clients: Vec::new(),
                    icmp_echo: false,
                }),
            },
        ));
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
            gateway: false,
        }
    }

    fn route(address: &str, prefix_len: u8) -> Ipv6Cidr {
        Ipv6Inet::new(address.parse::<Ipv6Addr>().unwrap(), prefix_len)
            .unwrap()
            .network()
    }

    fn ipv6_nat_config() -> Ipv6NatConfig {
        Ipv6NatConfig {
            gateway: Ipv6Inet::new("fd00::1".parse().unwrap(), 64).unwrap(),
            ra_preference: RaPreference::Medium,
        }
    }
}
