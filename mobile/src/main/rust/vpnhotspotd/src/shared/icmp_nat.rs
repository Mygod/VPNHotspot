use std::collections::{hash_map::Entry, HashMap};
use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};
use std::time::{Duration, Instant};

use crate::shared::model::Network;

pub const ICMPV6_DESTINATION_UNREACHABLE: u8 = 1;
pub const ICMPV6_PACKET_TOO_BIG: u8 = 2;
pub const ICMPV6_TIME_EXCEEDED: u8 = 3;
pub const ICMPV6_PARAMETER_PROBLEM: u8 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Nat66Destination {
    Gateway,
    Special,
    Routable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Nat66HopLimit {
    Missing,
    Expired,
    Forward(u8),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct EchoKey {
    network: Network,
    destination: Ipv6Addr,
    id: u16,
    seq: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EchoEntry {
    pub session_key: u64,
    pub downstream: String,
    pub reply_mark: u32,
    pub client: SocketAddrV6,
    pub original_id: u16,
    pub original_seq: u16,
    pub downstream_hop_limit: u8,
    pub upstream_hop_limit: u8,
    pub gateway: Ipv6Addr,
    created: Instant,
}

pub struct EchoAllocation {
    pub session_key: u64,
    pub downstream: String,
    pub reply_mark: u32,
    pub network: Network,
    pub destination: Ipv6Addr,
    pub client: SocketAddrV6,
    pub original_id: u16,
    pub original_seq: u16,
    pub downstream_hop_limit: u8,
    pub upstream_hop_limit: u8,
    pub gateway: Ipv6Addr,
}

#[derive(Default)]
pub struct EchoMap {
    next_id: u16,
    entries: HashMap<EchoKey, EchoEntry>,
}

pub fn classify_nat66_destination(destination: Ipv6Addr, gateway: Ipv6Addr) -> Nat66Destination {
    if destination == gateway {
        Nat66Destination::Gateway
    } else if is_special_destination(destination) {
        Nat66Destination::Special
    } else {
        Nat66Destination::Routable
    }
}

pub fn is_special_destination(destination: Ipv6Addr) -> bool {
    destination.is_multicast()
        || destination.is_unicast_link_local()
        || destination.is_loopback()
        || destination.is_unspecified()
}

pub fn nat66_hop_limit(hop_limit: Option<u8>) -> Nat66HopLimit {
    match hop_limit {
        Some(0 | 1) => Nat66HopLimit::Expired,
        Some(hop_limit) => Nat66HopLimit::Forward(hop_limit - 1),
        None => Nat66HopLimit::Missing,
    }
}

pub fn icmpv6_error_bytes(type_u8: u8, info: u32) -> Option<(u8, [u8; 4])> {
    match type_u8 {
        ICMPV6_DESTINATION_UNREACHABLE => Some((type_u8, [0; 4])),
        ICMPV6_PACKET_TOO_BIG => Some((type_u8, info.to_be_bytes())),
        ICMPV6_TIME_EXCEEDED => Some((type_u8, [0; 4])),
        ICMPV6_PARAMETER_PROBLEM => Some((type_u8, info.to_be_bytes())),
        _ => None,
    }
}

pub fn icmp_echo_rule_args(
    downstream: impl Into<String>,
    gateway: Ipv6Addr,
    queue_num: u16,
) -> Vec<String> {
    vec![
        "-i".into(),
        downstream.into(),
        "-p".into(),
        "icmpv6".into(),
        "--icmpv6-type".into(),
        "echo-request".into(),
        "!".into(),
        "-d".into(),
        gateway.to_string(),
        "-j".into(),
        "NFQUEUE".into(),
        "--queue-num".into(),
        queue_num.to_string(),
    ]
}

pub fn should_install_icmp_echo_rule(icmp_echo: bool) -> bool {
    icmp_echo
}

pub fn downstream_icmp_error_source(offender: Ipv6Addr, gateway: Ipv6Addr) -> Ipv6Addr {
    if offender.is_unicast_link_local() {
        gateway
    } else {
        offender
    }
}

impl EchoMap {
    pub fn allocate(
        &mut self,
        now: Instant,
        timeout: Duration,
        allocation: EchoAllocation,
    ) -> io::Result<(u16, u16)> {
        self.expire(now, timeout);
        for _ in 0..=u16::MAX {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            let key = EchoKey {
                network: allocation.network,
                destination: allocation.destination,
                id,
                seq: allocation.original_seq,
            };
            match self.entries.entry(key) {
                Entry::Occupied(_) => continue,
                Entry::Vacant(entry) => {
                    entry.insert(EchoEntry {
                        session_key: allocation.session_key,
                        downstream: allocation.downstream,
                        reply_mark: allocation.reply_mark,
                        client: allocation.client,
                        original_id: allocation.original_id,
                        original_seq: allocation.original_seq,
                        downstream_hop_limit: allocation.downstream_hop_limit,
                        upstream_hop_limit: allocation.upstream_hop_limit,
                        gateway: allocation.gateway,
                        created: now,
                    });
                    return Ok((id, allocation.original_seq));
                }
            }
        }
        Err(io::Error::other("exhausted echo identifiers"))
    }

    pub fn restore(
        &mut self,
        now: Instant,
        timeout: Duration,
        network: Network,
        source: Ipv6Addr,
        id: u16,
        seq: u16,
    ) -> Option<EchoEntry> {
        self.expire(now, timeout);
        self.entries.remove(&EchoKey {
            network,
            destination: source,
            id,
            seq,
        })
    }

    pub fn remove(
        &mut self,
        now: Instant,
        timeout: Duration,
        network: Network,
        destination: Ipv6Addr,
        id: u16,
        seq: u16,
    ) -> Option<EchoEntry> {
        self.expire(now, timeout);
        self.entries.remove(&EchoKey {
            network,
            destination,
            id,
            seq,
        })
    }

    pub fn contains(
        &mut self,
        now: Instant,
        timeout: Duration,
        network: Network,
        destination: Ipv6Addr,
        id: u16,
        seq: u16,
    ) -> bool {
        self.expire(now, timeout);
        self.entries.contains_key(&EchoKey {
            network,
            destination,
            id,
            seq,
        })
    }

    pub fn remove_session(&mut self, now: Instant, timeout: Duration, session_key: u64) {
        self.expire(now, timeout);
        self.entries
            .retain(|_, entry| entry.session_key != session_key);
    }

    pub fn has_network_entries(
        &mut self,
        now: Instant,
        timeout: Duration,
        network: Network,
    ) -> bool {
        self.expire(now, timeout);
        self.entries.keys().any(|key| key.network == network)
    }

    pub fn network_idle_deadline(
        &mut self,
        now: Instant,
        network: Network,
        timeout: Duration,
    ) -> Option<Instant> {
        self.expire(now, timeout);
        self.entries
            .iter()
            .filter(|(key, _)| key.network == network)
            .map(|(_, entry)| entry.created + timeout)
            .min()
    }

    fn expire(&mut self, now: Instant, timeout: Duration) {
        self.entries
            .retain(|_, entry| now.duration_since(entry.created) < timeout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::model::DAEMON_ICMP_NFQUEUE_NUM;

    const TIMEOUT: Duration = Duration::from_secs(60);

    #[test]
    fn destination_classifier_separates_gateway_special_and_routable() {
        let gateway = "fd00::1".parse().unwrap();
        assert_eq!(
            classify_nat66_destination(gateway, gateway),
            Nat66Destination::Gateway
        );
        for destination in ["ff02::1", "fe80::1", "::1", "::"] {
            assert_eq!(
                classify_nat66_destination(destination.parse().unwrap(), gateway),
                Nat66Destination::Special,
                "{destination}"
            );
        }
        assert_eq!(
            classify_nat66_destination("2001:db8::1".parse().unwrap(), gateway),
            Nat66Destination::Routable
        );
    }

    #[test]
    fn hop_limit_action_expires_boundary_packets_and_decrements_forwarded_packets() {
        assert_eq!(nat66_hop_limit(None), Nat66HopLimit::Missing);
        assert_eq!(nat66_hop_limit(Some(0)), Nat66HopLimit::Expired);
        assert_eq!(nat66_hop_limit(Some(1)), Nat66HopLimit::Expired);
        assert_eq!(nat66_hop_limit(Some(2)), Nat66HopLimit::Forward(1));
        assert_eq!(nat66_hop_limit(Some(64)), Nat66HopLimit::Forward(63));
    }

    #[test]
    fn icmpv6_error_metadata_matches_supported_types() {
        assert_eq!(
            icmpv6_error_bytes(ICMPV6_DESTINATION_UNREACHABLE, 1234),
            Some((1, [0; 4]))
        );
        assert_eq!(
            icmpv6_error_bytes(ICMPV6_PACKET_TOO_BIG, 1280),
            Some((2, 1280u32.to_be_bytes()))
        );
        assert_eq!(
            icmpv6_error_bytes(ICMPV6_TIME_EXCEEDED, 1234),
            Some((3, [0; 4]))
        );
        assert_eq!(
            icmpv6_error_bytes(ICMPV6_PARAMETER_PROBLEM, 9),
            Some((4, 9u32.to_be_bytes()))
        );
        assert_eq!(icmpv6_error_bytes(255, 0), None);
    }

    #[test]
    fn echo_map_restores_original_identity_once() {
        let mut map = EchoMap::default();
        let now = Instant::now();
        let destination = "2001:db8::1".parse().unwrap();
        let client = SocketAddrV6::new("fd00::2".parse().unwrap(), 0, 0, 0);

        let (id, seq) = map
            .allocate(
                now,
                TIMEOUT,
                EchoAllocation {
                    session_key: 1,
                    downstream: "ncm0".into(),
                    reply_mark: 123,
                    network: 12,
                    destination,
                    client,
                    original_id: 345,
                    original_seq: 678,
                    downstream_hop_limit: 64,
                    upstream_hop_limit: 63,
                    gateway: "fd00::1".parse().unwrap(),
                },
            )
            .unwrap();
        assert_eq!(seq, 678);

        let entry = map.restore(now, TIMEOUT, 12, destination, id, seq).unwrap();
        assert_eq!(entry.session_key, 1);
        assert_eq!(entry.downstream, "ncm0");
        assert_eq!(entry.reply_mark, 123);
        assert_eq!(entry.client, client);
        assert_eq!(entry.original_id, 345);
        assert_eq!(entry.original_seq, 678);
        assert_eq!(entry.downstream_hop_limit, 64);
        assert_eq!(entry.upstream_hop_limit, 63);
        assert_eq!(entry.gateway, "fd00::1".parse::<Ipv6Addr>().unwrap());
        assert!(map
            .restore(now, TIMEOUT, 12, destination, id, seq)
            .is_none());
    }

    #[test]
    fn echo_map_expires_old_entries_before_restore() {
        let mut map = EchoMap::default();
        let destination = "2001:db8::1".parse().unwrap();
        let client = SocketAddrV6::new("fd00::2".parse().unwrap(), 0, 0, 0);
        let created = Instant::now() - TIMEOUT - Duration::from_secs(1);
        let (id, seq) = map
            .allocate(
                created,
                TIMEOUT,
                EchoAllocation {
                    session_key: 1,
                    downstream: "ncm0".into(),
                    reply_mark: 123,
                    network: 12,
                    destination,
                    client,
                    original_id: 1,
                    original_seq: 2,
                    downstream_hop_limit: 64,
                    upstream_hop_limit: 63,
                    gateway: "fd00::1".parse().unwrap(),
                },
            )
            .unwrap();

        assert!(map
            .restore(Instant::now(), TIMEOUT, 12, destination, id, seq)
            .is_none());
    }

    #[test]
    fn echo_map_removes_session_entries() {
        let mut map = EchoMap::default();
        let now = Instant::now();
        let destination: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let client = SocketAddrV6::new("fd00::2".parse().unwrap(), 0, 0, 0);
        let (removed_id, removed_seq) = map
            .allocate(
                now,
                TIMEOUT,
                EchoAllocation {
                    session_key: 1,
                    downstream: "ncm0".into(),
                    reply_mark: 123,
                    network: 12,
                    destination,
                    client,
                    original_id: 1,
                    original_seq: 2,
                    downstream_hop_limit: 64,
                    upstream_hop_limit: 63,
                    gateway: "fd00::1".parse().unwrap(),
                },
            )
            .unwrap();
        let (kept_id, kept_seq) = map
            .allocate(
                now,
                TIMEOUT,
                EchoAllocation {
                    session_key: 2,
                    downstream: "wlan0".into(),
                    reply_mark: 456,
                    network: 12,
                    destination,
                    client,
                    original_id: 3,
                    original_seq: 4,
                    downstream_hop_limit: 64,
                    upstream_hop_limit: 63,
                    gateway: "fd00::1".parse().unwrap(),
                },
            )
            .unwrap();

        map.remove_session(now, TIMEOUT, 1);

        assert!(map
            .restore(now, TIMEOUT, 12, destination, removed_id, removed_seq)
            .is_none());
        assert!(map
            .restore(now, TIMEOUT, 12, destination, kept_id, kept_seq)
            .is_some());
    }

    #[test]
    fn echo_map_removes_specific_allocation() {
        let mut map = EchoMap::default();
        let now = Instant::now();
        let destination: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let client = SocketAddrV6::new("fd00::2".parse().unwrap(), 0, 0, 0);
        let (id, seq) = map
            .allocate(
                now,
                TIMEOUT,
                EchoAllocation {
                    session_key: 1,
                    downstream: "ncm0".into(),
                    reply_mark: 123,
                    network: 12,
                    destination,
                    client,
                    original_id: 1,
                    original_seq: 2,
                    downstream_hop_limit: 64,
                    upstream_hop_limit: 63,
                    gateway: "fd00::1".parse().unwrap(),
                },
            )
            .unwrap();

        assert!(map.contains(now, TIMEOUT, 12, destination, id, seq));
        assert!(map.remove(now, TIMEOUT, 12, destination, id, seq).is_some());
        assert!(!map.contains(now, TIMEOUT, 12, destination, id, seq));
        assert!(map
            .restore(now, TIMEOUT, 12, destination, id, seq)
            .is_none());
    }

    #[test]
    fn echo_map_tracks_live_networks_after_expiry() {
        let mut map = EchoMap::default();
        let now = Instant::now();
        let destination: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let client = SocketAddrV6::new("fd00::2".parse().unwrap(), 0, 0, 0);
        map.allocate(
            now - TIMEOUT - Duration::from_secs(1),
            TIMEOUT,
            EchoAllocation {
                session_key: 1,
                downstream: "ncm0".into(),
                reply_mark: 123,
                network: 12,
                destination,
                client,
                original_id: 1,
                original_seq: 2,
                downstream_hop_limit: 64,
                upstream_hop_limit: 63,
                gateway: "fd00::1".parse().unwrap(),
            },
        )
        .unwrap();
        map.allocate(
            now,
            TIMEOUT,
            EchoAllocation {
                session_key: 1,
                downstream: "ncm0".into(),
                reply_mark: 123,
                network: 13,
                destination,
                client,
                original_id: 3,
                original_seq: 4,
                downstream_hop_limit: 64,
                upstream_hop_limit: 63,
                gateway: "fd00::1".parse().unwrap(),
            },
        )
        .unwrap();

        assert!(!map.has_network_entries(now, TIMEOUT, 12));
        assert!(map.has_network_entries(now, TIMEOUT, 13));
    }

    #[test]
    fn echo_map_reports_network_idle_deadline() {
        let mut map = EchoMap::default();
        let now = Instant::now();
        let destination: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let client = SocketAddrV6::new("fd00::2".parse().unwrap(), 0, 0, 0);
        map.allocate(
            now,
            TIMEOUT,
            EchoAllocation {
                session_key: 1,
                downstream: "ncm0".into(),
                reply_mark: 123,
                network: 12,
                destination,
                client,
                original_id: 1,
                original_seq: 2,
                downstream_hop_limit: 64,
                upstream_hop_limit: 63,
                gateway: "fd00::1".parse().unwrap(),
            },
        )
        .unwrap();

        assert_eq!(
            map.network_idle_deadline(now, 12, TIMEOUT),
            Some(now + TIMEOUT)
        );
        assert_eq!(map.network_idle_deadline(now, 13, TIMEOUT), None);
    }

    #[test]
    fn icmp_echo_rule_args_exclude_gateway_and_queue_echo_requests() {
        assert_eq!(
            icmp_echo_rule_args("ncm0", "fd00::1".parse().unwrap(), DAEMON_ICMP_NFQUEUE_NUM),
            vec![
                "-i",
                "ncm0",
                "-p",
                "icmpv6",
                "--icmpv6-type",
                "echo-request",
                "!",
                "-d",
                "fd00::1",
                "-j",
                "NFQUEUE",
                "--queue-num",
                "30063",
            ]
        );
    }

    #[test]
    fn icmp_echo_route_installation_follows_startup_capability() {
        assert!(should_install_icmp_echo_rule(true));
        assert!(!should_install_icmp_echo_rule(false));
    }

    #[test]
    fn downstream_icmp_error_source_rewrites_link_local_offenders_to_gateway() {
        let gateway = "fd00::1".parse().unwrap();
        assert_eq!(
            downstream_icmp_error_source("fe80::1234".parse().unwrap(), gateway),
            gateway
        );
        assert_eq!(
            downstream_icmp_error_source("2001:db8::1".parse().unwrap(), gateway),
            "2001:db8::1".parse::<Ipv6Addr>().unwrap()
        );
    }
}
