use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::rtnetlink;
use vpnhotspotd::shared::model::{
    ClientConfig, Ipv6NatConfig, MasqueradeMode, SessionConfig, SessionPorts, UpstreamConfig,
    UpstreamRole, DAEMON_INTERCEPT_FWMARK_MASK, DAEMON_INTERCEPT_FWMARK_VALUE, DAEMON_REPLY_MARK,
    DAEMON_REPLY_MARK_MASK, DAEMON_TABLE, LOCAL_NETWORK_TABLE,
};
use vpnhotspotd::shared::protocol::{
    CleanIpCommand, IpAddressCommand, IpFamily, IpOperation, IpRouteCommand, IpRuleCommand,
    RouteType, RuleAction,
};

const IPTABLES: &str = "iptables";
const IP6TABLES: &str = "ip6tables";
const IPTABLES_RESTORE: &str = "iptables-restore";
const IP6TABLES_RESTORE: &str = "ip6tables-restore";

const RULE_PRIORITY_DAEMON_BASE: u32 = 20600;
const RULE_PRIORITY_UPSTREAM_BASE: u32 = 20700;
const RULE_PRIORITY_UPSTREAM_FALLBACK_BASE: u32 = 20800;
const RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE: u32 = 20900;

pub(crate) struct Runtime {
    ports: SessionPorts,
}

impl Runtime {
    pub(crate) async fn start(config: &SessionConfig, ports: SessionPorts) -> io::Result<Self> {
        let runtime = Self { ports };
        if let Err(e) = runtime.setup(config).await {
            runtime.remove(config).await;
            return Err(e);
        }
        Ok(runtime)
    }

    pub(crate) async fn replace(
        &mut self,
        previous: &SessionConfig,
        next: &SessionConfig,
    ) -> io::Result<()> {
        if previous.downstream != next.downstream {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "routing session downstream cannot change",
            ));
        }
        if previous.dns_bind_address != next.dns_bind_address {
            self.remove_dns(previous).await;
            self.add_dns(next).await?;
        }
        if previous.ip_forward != next.ip_forward {
            if previous.ip_forward {
                self.remove_ip_forward(previous).await;
            }
            if next.ip_forward {
                self.add_ip_forward(next).await?;
            }
        }
        if previous.forward != next.forward {
            if previous.forward {
                self.remove_forward(previous).await;
            }
            if next.forward {
                self.add_forward(next).await?;
            }
        }
        if !(previous.ipv6_nat.is_none() && next.ipv6_nat.is_some()) {
            self.replace_clients(previous, next).await?;
        }
        if previous.masquerade != next.masquerade && next.masquerade == MasqueradeMode::Simple {
            self.add_masquerade_chain(next).await?;
        }
        self.replace_upstreams(previous, next).await?;
        if previous.masquerade != next.masquerade && previous.masquerade == MasqueradeMode::Simple {
            self.remove_masquerade_chain(previous).await;
        }
        if previous.ipv6_block != next.ipv6_block {
            if previous.ipv6_block {
                self.remove_ipv6_block(previous).await;
            }
            if next.ipv6_block {
                self.add_ipv6_block(next).await?;
            }
        }
        if previous.ipv6_nat != next.ipv6_nat {
            if let Some(ipv6_nat) = &previous.ipv6_nat {
                self.remove_ipv6_nat(previous, ipv6_nat).await;
            }
            if let Some(ipv6_nat) = &next.ipv6_nat {
                self.add_ipv6_nat(next, ipv6_nat).await?;
            }
        }
        if previous.ipv6_nat.is_none() && next.ipv6_nat.is_some() {
            self.replace_clients(previous, next).await?;
        }
        Ok(())
    }

    pub(crate) async fn stop(self, config: &SessionConfig) {
        self.remove(config).await;
    }

    async fn setup(&self, config: &SessionConfig) -> io::Result<()> {
        if config.ip_forward {
            self.add_ip_forward(config).await?;
        }
        self.add_dns(config).await?;
        self.add_disable_system_rule(config).await?;
        if config.forward {
            self.add_forward(config).await?;
        }
        if config.masquerade == MasqueradeMode::Simple {
            self.add_masquerade_chain(config).await?;
        }
        for upstream in &config.upstreams {
            self.add_upstream(config, upstream).await?;
        }
        if config.ipv6_block {
            self.add_ipv6_block(config).await?;
        }
        if let Some(ipv6_nat) = &config.ipv6_nat {
            self.add_ipv6_nat(config, ipv6_nat).await?;
        }
        for client in &config.clients {
            self.add_client(config, client).await?;
        }
        Ok(())
    }

    async fn remove(&self, config: &SessionConfig) {
        for client in &config.clients {
            self.remove_client(config, client).await;
        }
        if let Some(ipv6_nat) = &config.ipv6_nat {
            self.remove_ipv6_nat(config, ipv6_nat).await;
        }
        if config.ipv6_block {
            self.remove_ipv6_block(config).await;
        }
        for upstream in &config.upstreams {
            self.remove_upstream(config, upstream).await;
        }
        if config.masquerade == MasqueradeMode::Simple {
            self.remove_masquerade_chain(config).await;
        }
        if config.forward {
            self.remove_forward(config).await;
        }
        self.remove_disable_system_rule(config).await;
        self.remove_dns(config).await;
        if config.ip_forward {
            self.remove_ip_forward(config).await;
        }
    }

    async fn replace_clients(
        &self,
        previous: &SessionConfig,
        next: &SessionConfig,
    ) -> io::Result<()> {
        let previous_macs = client_macs(previous);
        let next_macs = client_macs(next);
        let previous_ips = client_ips(previous);
        let next_ips = client_ips(next);
        if previous.forward {
            for mac in previous_macs.difference(&next_macs) {
                self.remove_client_mac_v4(
                    previous,
                    &ClientConfig {
                        mac: *mac,
                        ipv4: Vec::new(),
                    },
                )
                .await;
            }
            for address in previous_ips.difference(&next_ips) {
                self.remove_client_ip_stats(previous, *address).await;
            }
            if !next.forward {
                for mac in previous_macs.intersection(&next_macs) {
                    self.remove_client_mac_v4(
                        previous,
                        &ClientConfig {
                            mac: *mac,
                            ipv4: Vec::new(),
                        },
                    )
                    .await;
                }
                for address in previous_ips.intersection(&next_ips) {
                    self.remove_client_ip_stats(previous, *address).await;
                }
            }
        }
        if next.forward {
            for mac in next_macs.difference(&previous_macs) {
                self.add_client_mac_v4(
                    next,
                    &ClientConfig {
                        mac: *mac,
                        ipv4: Vec::new(),
                    },
                )
                .await?;
            }
            for address in next_ips.difference(&previous_ips) {
                self.add_client_ip_stats(next, *address).await?;
            }
            if !previous.forward {
                for mac in previous_macs.intersection(&next_macs) {
                    self.add_client_mac_v4(
                        next,
                        &ClientConfig {
                            mac: *mac,
                            ipv4: Vec::new(),
                        },
                    )
                    .await?;
                }
                for address in previous_ips.intersection(&next_ips) {
                    self.add_client_ip_stats(next, *address).await?;
                }
            }
        }
        if previous.ipv6_nat.is_some() {
            for mac in previous_macs.difference(&next_macs) {
                self.remove_client_mac_v6(&ClientConfig {
                    mac: *mac,
                    ipv4: Vec::new(),
                })
                .await;
            }
            if next.ipv6_nat.is_none() {
                for mac in previous_macs.intersection(&next_macs) {
                    self.remove_client_mac_v6(&ClientConfig {
                        mac: *mac,
                        ipv4: Vec::new(),
                    })
                    .await;
                }
            }
        }
        if next.ipv6_nat.is_some() {
            for mac in next_macs.difference(&previous_macs) {
                self.add_client_mac_v6(&ClientConfig {
                    mac: *mac,
                    ipv4: Vec::new(),
                })
                .await?;
            }
            if previous.ipv6_nat.is_none() {
                for mac in previous_macs.intersection(&next_macs) {
                    self.add_client_mac_v6(&ClientConfig {
                        mac: *mac,
                        ipv4: Vec::new(),
                    })
                    .await?;
                }
            }
        }
        Ok(())
    }

    async fn replace_upstreams(
        &self,
        previous: &SessionConfig,
        next: &SessionConfig,
    ) -> io::Result<()> {
        let previous_upstreams: HashSet<_> = previous.upstreams.iter().collect();
        let next_upstreams: HashSet<_> = next.upstreams.iter().collect();
        for upstream in previous_upstreams.difference(&next_upstreams) {
            self.remove_upstream(previous, upstream).await;
        }
        if previous.masquerade != next.masquerade {
            for upstream in previous_upstreams.intersection(&next_upstreams) {
                self.remove_upstream_masquerade(previous, upstream).await;
            }
        }
        for upstream in next_upstreams.difference(&previous_upstreams) {
            self.add_upstream(next, upstream).await?;
        }
        if previous.masquerade != next.masquerade {
            for upstream in previous_upstreams.intersection(&next_upstreams) {
                self.add_upstream_masquerade(next, upstream).await?;
            }
        }
        Ok(())
    }

    async fn add_dns(&self, config: &SessionConfig) -> io::Result<()> {
        self.iptables_insert(
            IPTABLES,
            "nat",
            "PREROUTING",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-p".into(),
                "tcp".into(),
                "-d".into(),
                config.dns_bind_address.to_string(),
                "--dport".into(),
                "53".into(),
                "-j".into(),
                "DNAT".into(),
                "--to-destination".into(),
                format!(":{}", self.ports.dns_tcp),
            ],
        )
        .await?;
        self.iptables_insert(
            IPTABLES,
            "nat",
            "PREROUTING",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-p".into(),
                "udp".into(),
                "-d".into(),
                config.dns_bind_address.to_string(),
                "--dport".into(),
                "53".into(),
                "-j".into(),
                "DNAT".into(),
                "--to-destination".into(),
                format!(":{}", self.ports.dns_udp),
            ],
        )
        .await
    }

    async fn remove_dns(&self, config: &SessionConfig) {
        self.iptables_delete(
            IPTABLES,
            "nat",
            "PREROUTING",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-p".into(),
                "tcp".into(),
                "-d".into(),
                config.dns_bind_address.to_string(),
                "--dport".into(),
                "53".into(),
                "-j".into(),
                "DNAT".into(),
                "--to-destination".into(),
                format!(":{}", self.ports.dns_tcp),
            ],
        )
        .await;
        self.iptables_delete(
            IPTABLES,
            "nat",
            "PREROUTING",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-p".into(),
                "udp".into(),
                "-d".into(),
                config.dns_bind_address.to_string(),
                "--dport".into(),
                "53".into(),
                "-j".into(),
                "DNAT".into(),
                "--to-destination".into(),
                format!(":{}", self.ports.dns_udp),
            ],
        )
        .await;
    }

    async fn add_ip_forward(&self, config: &SessionConfig) -> io::Result<()> {
        if let Err(e) = run_ndc(
            "ipfwd",
            &[
                "ipfwd",
                "enable",
                &format!("vpnhotspot_{}", config.downstream),
            ],
        )
        .await
        {
            eprintln!("ndc ipfwd enable failed: {e}");
            tokio::fs::write("/proc/sys/net/ipv4/ip_forward", b"1").await?;
        }
        Ok(())
    }

    async fn remove_ip_forward(&self, config: &SessionConfig) {
        let _ = run_ndc(
            "ipfwd",
            &[
                "ipfwd",
                "disable",
                &format!("vpnhotspot_{}", config.downstream),
            ],
        )
        .await;
    }

    async fn add_disable_system_rule(&self, config: &SessionConfig) -> io::Result<()> {
        add_rule(IpRuleCommand {
            operation: IpOperation::Replace,
            family: IpFamily::Ipv4,
            iif: config.downstream.clone(),
            priority: rule_priority(RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE),
            action: RuleAction::Unreachable,
            table: 0,
            fwmark: None,
        })
        .await
    }

    async fn remove_disable_system_rule(&self, config: &SessionConfig) {
        delete_rule(IpRuleCommand {
            operation: IpOperation::Delete,
            family: IpFamily::Ipv4,
            iif: config.downstream.clone(),
            priority: rule_priority(RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE),
            action: RuleAction::Unreachable,
            table: 0,
            fwmark: None,
        })
        .await;
    }

    async fn add_forward(&self, config: &SessionConfig) -> io::Result<()> {
        self.iptables_new_chain(IPTABLES, "filter", "vpnhotspot_acl")
            .await;
        self.iptables_new_chain(IPTABLES, "filter", "vpnhotspot_stats")
            .await;
        self.iptables_insert(
            IPTABLES,
            "filter",
            "FORWARD",
            &["-j".into(), "vpnhotspot_acl".into()],
        )
        .await?;
        self.iptables_insert(
            IPTABLES,
            "filter",
            "vpnhotspot_acl",
            &[
                "-i".into(),
                config.downstream.clone(),
                "!".into(),
                "-o".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IPTABLES,
            "filter",
            "vpnhotspot_acl",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-m".into(),
                "state".into(),
                "--state".into(),
                "ESTABLISHED,RELATED".into(),
                "-j".into(),
                "ACCEPT".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IPTABLES,
            "filter",
            "vpnhotspot_acl",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-m".into(),
                "state".into(),
                "--state".into(),
                "ESTABLISHED,RELATED".into(),
                "-j".into(),
                "vpnhotspot_stats".into(),
            ],
        )
        .await
    }

    async fn remove_forward(&self, config: &SessionConfig) {
        self.iptables_delete(
            IPTABLES,
            "filter",
            "FORWARD",
            &["-j".into(), "vpnhotspot_acl".into()],
        )
        .await;
        self.iptables_delete(
            IPTABLES,
            "filter",
            "vpnhotspot_acl",
            &[
                "-i".into(),
                config.downstream.clone(),
                "!".into(),
                "-o".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await;
        self.iptables_delete(
            IPTABLES,
            "filter",
            "vpnhotspot_acl",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-m".into(),
                "state".into(),
                "--state".into(),
                "ESTABLISHED,RELATED".into(),
                "-j".into(),
                "ACCEPT".into(),
            ],
        )
        .await;
        self.iptables_delete(
            IPTABLES,
            "filter",
            "vpnhotspot_acl",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-m".into(),
                "state".into(),
                "--state".into(),
                "ESTABLISHED,RELATED".into(),
                "-j".into(),
                "vpnhotspot_stats".into(),
            ],
        )
        .await;
    }

    async fn add_masquerade_chain(&self, _config: &SessionConfig) -> io::Result<()> {
        self.iptables_new_chain(IPTABLES, "nat", "vpnhotspot_masquerade")
            .await;
        self.iptables_insert(
            IPTABLES,
            "nat",
            "POSTROUTING",
            &["-j".into(), "vpnhotspot_masquerade".into()],
        )
        .await
    }

    async fn remove_masquerade_chain(&self, _config: &SessionConfig) {
        self.iptables_delete(
            IPTABLES,
            "nat",
            "POSTROUTING",
            &["-j".into(), "vpnhotspot_masquerade".into()],
        )
        .await;
    }

    async fn add_upstream(
        &self,
        config: &SessionConfig,
        upstream: &UpstreamConfig,
    ) -> io::Result<()> {
        add_rule(IpRuleCommand {
            operation: IpOperation::Replace,
            family: IpFamily::Ipv4,
            iif: config.downstream.clone(),
            priority: rule_priority(match upstream.role {
                UpstreamRole::Primary => RULE_PRIORITY_UPSTREAM_BASE,
                UpstreamRole::Fallback => RULE_PRIORITY_UPSTREAM_FALLBACK_BASE,
            }),
            action: RuleAction::Lookup,
            table: 1000 + upstream.ifindex,
            fwmark: None,
        })
        .await?;
        self.add_upstream_masquerade(config, upstream).await
    }

    async fn remove_upstream(&self, config: &SessionConfig, upstream: &UpstreamConfig) {
        self.remove_upstream_masquerade(config, upstream).await;
        delete_rule(IpRuleCommand {
            operation: IpOperation::Delete,
            family: IpFamily::Ipv4,
            iif: config.downstream.clone(),
            priority: rule_priority(match upstream.role {
                UpstreamRole::Primary => RULE_PRIORITY_UPSTREAM_BASE,
                UpstreamRole::Fallback => RULE_PRIORITY_UPSTREAM_FALLBACK_BASE,
            }),
            action: RuleAction::Lookup,
            table: 1000 + upstream.ifindex,
            fwmark: None,
        })
        .await;
    }

    async fn add_upstream_masquerade(
        &self,
        config: &SessionConfig,
        upstream: &UpstreamConfig,
    ) -> io::Result<()> {
        match config.masquerade {
            MasqueradeMode::None => Ok(()),
            MasqueradeMode::Simple => {
                self.iptables_insert(
                    IPTABLES,
                    "nat",
                    "vpnhotspot_masquerade",
                    &[
                        "-s".into(),
                        host_subnet(config),
                        "-o".into(),
                        upstream.ifname.clone(),
                        "-j".into(),
                        "MASQUERADE".into(),
                    ],
                )
                .await
            }
            MasqueradeMode::Netd => {
                run_ndc(
                    "Nat",
                    &["nat", "enable", &config.downstream, &upstream.ifname, "0"],
                )
                .await
            }
        }
    }

    async fn remove_upstream_masquerade(&self, config: &SessionConfig, upstream: &UpstreamConfig) {
        match config.masquerade {
            MasqueradeMode::None => {}
            MasqueradeMode::Simple => {
                self.iptables_delete(
                    IPTABLES,
                    "nat",
                    "vpnhotspot_masquerade",
                    &[
                        "-s".into(),
                        host_subnet(config),
                        "-o".into(),
                        upstream.ifname.clone(),
                        "-j".into(),
                        "MASQUERADE".into(),
                    ],
                )
                .await;
            }
            MasqueradeMode::Netd => {
                let _ = run_ndc(
                    "Nat",
                    &["nat", "disable", &config.downstream, &upstream.ifname, "0"],
                )
                .await;
            }
        }
    }

    async fn add_client(&self, config: &SessionConfig, client: &ClientConfig) -> io::Result<()> {
        if config.forward {
            self.add_client_v4(config, client).await?;
        }
        if config.ipv6_nat.is_some() {
            self.add_client_mac_v6(client).await?;
        }
        Ok(())
    }

    async fn remove_client(&self, config: &SessionConfig, client: &ClientConfig) {
        if config.ipv6_nat.is_some() {
            self.remove_client_mac_v6(client).await;
        }
        if config.forward {
            self.remove_client_v4(config, client).await;
        }
    }

    async fn add_client_v4(&self, config: &SessionConfig, client: &ClientConfig) -> io::Result<()> {
        self.add_client_mac_v4(config, client).await?;
        for address in &client.ipv4 {
            self.add_client_ip_stats(config, *address).await?;
        }
        Ok(())
    }

    async fn remove_client_v4(&self, config: &SessionConfig, client: &ClientConfig) {
        for address in &client.ipv4 {
            self.remove_client_ip_stats(config, *address).await;
        }
        self.remove_client_mac_v4(config, client).await;
    }

    async fn add_client_mac_v4(
        &self,
        config: &SessionConfig,
        client: &ClientConfig,
    ) -> io::Result<()> {
        let mac = mac_string(&client.mac);
        self.iptables_insert(
            IPTABLES,
            "filter",
            "vpnhotspot_acl",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-m".into(),
                "mac".into(),
                "--mac-source".into(),
                mac.clone(),
                "-j".into(),
                "ACCEPT".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IPTABLES,
            "filter",
            "vpnhotspot_acl",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-m".into(),
                "mac".into(),
                "--mac-source".into(),
                mac,
                "-j".into(),
                "vpnhotspot_stats".into(),
            ],
        )
        .await
    }

    async fn remove_client_mac_v4(&self, config: &SessionConfig, client: &ClientConfig) {
        let mac = mac_string(&client.mac);
        self.iptables_delete(
            IPTABLES,
            "filter",
            "vpnhotspot_acl",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-m".into(),
                "mac".into(),
                "--mac-source".into(),
                mac.clone(),
                "-j".into(),
                "vpnhotspot_stats".into(),
            ],
        )
        .await;
        self.iptables_delete(
            IPTABLES,
            "filter",
            "vpnhotspot_acl",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-m".into(),
                "mac".into(),
                "--mac-source".into(),
                mac,
                "-j".into(),
                "ACCEPT".into(),
            ],
        )
        .await;
    }

    async fn add_client_ip_stats(
        &self,
        config: &SessionConfig,
        address: Ipv4Addr,
    ) -> io::Result<()> {
        self.iptables_insert(
            IPTABLES,
            "filter",
            "vpnhotspot_stats",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-s".into(),
                address.to_string(),
                "-j".into(),
                "RETURN".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IPTABLES,
            "filter",
            "vpnhotspot_stats",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-d".into(),
                address.to_string(),
                "-j".into(),
                "RETURN".into(),
            ],
        )
        .await
    }

    async fn remove_client_ip_stats(&self, config: &SessionConfig, address: Ipv4Addr) {
        self.iptables_delete(
            IPTABLES,
            "filter",
            "vpnhotspot_stats",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-s".into(),
                address.to_string(),
                "-j".into(),
                "RETURN".into(),
            ],
        )
        .await;
        self.iptables_delete(
            IPTABLES,
            "filter",
            "vpnhotspot_stats",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-d".into(),
                address.to_string(),
                "-j".into(),
                "RETURN".into(),
            ],
        )
        .await;
    }

    async fn add_client_mac_v6(&self, client: &ClientConfig) -> io::Result<()> {
        self.iptables_insert(
            IP6TABLES,
            "mangle",
            "vpnhotspot_acl",
            &[
                "-m".into(),
                "mac".into(),
                "--mac-source".into(),
                mac_string(&client.mac),
                "-j".into(),
                "RETURN".into(),
            ],
        )
        .await
    }

    async fn remove_client_mac_v6(&self, client: &ClientConfig) {
        self.iptables_delete(
            IP6TABLES,
            "mangle",
            "vpnhotspot_acl",
            &[
                "-m".into(),
                "mac".into(),
                "--mac-source".into(),
                mac_string(&client.mac),
                "-j".into(),
                "RETURN".into(),
            ],
        )
        .await;
    }

    async fn add_ipv6_block(&self, config: &SessionConfig) -> io::Result<()> {
        self.iptables_new_chain(IP6TABLES, "filter", "vpnhotspot_filter")
            .await;
        for chain in ["INPUT", "FORWARD", "OUTPUT"] {
            self.iptables_insert(
                IP6TABLES,
                "filter",
                chain,
                &["-j".into(), "vpnhotspot_filter".into()],
            )
            .await?;
        }
        self.iptables_insert(
            IP6TABLES,
            "filter",
            "vpnhotspot_filter",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IP6TABLES,
            "filter",
            "vpnhotspot_filter",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await
    }

    async fn remove_ipv6_block(&self, config: &SessionConfig) {
        for chain in ["INPUT", "FORWARD", "OUTPUT"] {
            self.iptables_delete(
                IP6TABLES,
                "filter",
                chain,
                &["-j".into(), "vpnhotspot_filter".into()],
            )
            .await;
        }
        self.iptables_delete(
            IP6TABLES,
            "filter",
            "vpnhotspot_filter",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await;
        self.iptables_delete(
            IP6TABLES,
            "filter",
            "vpnhotspot_filter",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await;
    }

    async fn add_ipv6_nat(
        &self,
        config: &SessionConfig,
        ipv6_nat: &Ipv6NatConfig,
    ) -> io::Result<()> {
        let ports = self
            .ports
            .ipv6_nat
            .ok_or_else(|| io::Error::other("missing IPv6 NAT ports"))?;
        apply_route(IpRouteCommand {
            operation: IpOperation::Replace,
            route_type: RouteType::Unicast,
            destination: IpAddr::V6(route_address(ipv6_nat.gateway, ipv6_nat.prefix_len)),
            prefix_len: ipv6_nat.prefix_len,
            interface: config.downstream.clone(),
            table: LOCAL_NETWORK_TABLE,
        })
        .await?;
        apply_address(IpAddressCommand {
            operation: IpOperation::Replace,
            address: IpAddr::V6(ipv6_nat.gateway),
            prefix_len: ipv6_nat.prefix_len,
            interface: config.downstream.clone(),
        })
        .await?;
        apply_route(IpRouteCommand {
            operation: IpOperation::Replace,
            route_type: RouteType::Local,
            destination: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            prefix_len: 0,
            interface: "lo".to_string(),
            table: DAEMON_TABLE,
        })
        .await?;
        add_rule(IpRuleCommand {
            operation: IpOperation::Replace,
            family: IpFamily::Ipv6,
            iif: config.downstream.clone(),
            priority: rule_priority(RULE_PRIORITY_DAEMON_BASE),
            action: RuleAction::Lookup,
            table: DAEMON_TABLE,
            fwmark: Some((DAEMON_INTERCEPT_FWMARK_VALUE, DAEMON_INTERCEPT_FWMARK_MASK)),
        })
        .await?;
        for chain in [
            "vpnhotspot_v6_input",
            "vpnhotspot_v6_forward",
            "vpnhotspot_v6_output",
        ] {
            self.iptables_new_chain(IP6TABLES, "filter", chain).await;
        }
        self.iptables_new_chain(IP6TABLES, "mangle", "vpnhotspot_acl")
            .await;
        self.iptables_new_chain(IP6TABLES, "mangle", "vpnhotspot_v6_tproxy")
            .await;
        self.iptables_insert(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_input",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_input",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-m".into(),
                "socket".into(),
                "--transparent".into(),
                "--nowildcard".into(),
                "-j".into(),
                "ACCEPT".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_input",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-p".into(),
                "icmpv6".into(),
                "-j".into(),
                "ACCEPT".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_forward",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_forward",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_output",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-p".into(),
                "icmpv6".into(),
                "--icmpv6-type".into(),
                "134".into(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_output",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-p".into(),
                "icmpv6".into(),
                "--icmpv6-type".into(),
                "134".into(),
                "-m".into(),
                "mark".into(),
                "--mark".into(),
                format!("{DAEMON_REPLY_MARK}/{DAEMON_REPLY_MARK_MASK}"),
                "-j".into(),
                "ACCEPT".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IP6TABLES,
            "mangle",
            "vpnhotspot_acl",
            &["-j".into(), "DROP".into()],
        )
        .await?;
        self.iptables_insert(
            IP6TABLES,
            "mangle",
            "vpnhotspot_v6_tproxy",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-p".into(),
                "tcp".into(),
                "-j".into(),
                "TPROXY".into(),
                "--on-port".into(),
                ports.tcp.to_string(),
                "--tproxy-mark".into(),
                "0x10000000/0x10000000".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IP6TABLES,
            "mangle",
            "vpnhotspot_v6_tproxy",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-p".into(),
                "udp".into(),
                "-j".into(),
                "TPROXY".into(),
                "--on-port".into(),
                ports.udp.to_string(),
                "--tproxy-mark".into(),
                "0x10000000/0x10000000".into(),
            ],
        )
        .await?;
        self.iptables_insert(
            IP6TABLES,
            "mangle",
            "vpnhotspot_v6_tproxy",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-j".into(),
                "vpnhotspot_acl".into(),
            ],
        )
        .await?;
        for (chain, target) in [
            ("INPUT", "vpnhotspot_v6_input"),
            ("FORWARD", "vpnhotspot_v6_forward"),
            ("OUTPUT", "vpnhotspot_v6_output"),
        ] {
            self.iptables_insert(IP6TABLES, "filter", chain, &["-j".into(), target.into()])
                .await?;
        }
        self.iptables_insert(
            IP6TABLES,
            "mangle",
            "PREROUTING",
            &["-j".into(), "vpnhotspot_v6_tproxy".into()],
        )
        .await
    }

    async fn remove_ipv6_nat(&self, config: &SessionConfig, ipv6_nat: &Ipv6NatConfig) {
        if let Some(ports) = self.ports.ipv6_nat {
            self.iptables_delete(
                IP6TABLES,
                "mangle",
                "vpnhotspot_v6_tproxy",
                &[
                    "-i".into(),
                    config.downstream.clone(),
                    "-p".into(),
                    "tcp".into(),
                    "-j".into(),
                    "TPROXY".into(),
                    "--on-port".into(),
                    ports.tcp.to_string(),
                    "--tproxy-mark".into(),
                    "0x10000000/0x10000000".into(),
                ],
            )
            .await;
            self.iptables_delete(
                IP6TABLES,
                "mangle",
                "vpnhotspot_v6_tproxy",
                &[
                    "-i".into(),
                    config.downstream.clone(),
                    "-p".into(),
                    "udp".into(),
                    "-j".into(),
                    "TPROXY".into(),
                    "--on-port".into(),
                    ports.udp.to_string(),
                    "--tproxy-mark".into(),
                    "0x10000000/0x10000000".into(),
                ],
            )
            .await;
        }
        self.iptables_delete(
            IP6TABLES,
            "mangle",
            "vpnhotspot_v6_tproxy",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-j".into(),
                "vpnhotspot_acl".into(),
            ],
        )
        .await;
        self.iptables_delete(
            IP6TABLES,
            "mangle",
            "vpnhotspot_acl",
            &["-j".into(), "DROP".into()],
        )
        .await;
        for (chain, target) in [
            ("INPUT", "vpnhotspot_v6_input"),
            ("FORWARD", "vpnhotspot_v6_forward"),
            ("OUTPUT", "vpnhotspot_v6_output"),
        ] {
            self.iptables_delete(IP6TABLES, "filter", chain, &["-j".into(), target.into()])
                .await;
        }
        self.iptables_delete(
            IP6TABLES,
            "mangle",
            "PREROUTING",
            &["-j".into(), "vpnhotspot_v6_tproxy".into()],
        )
        .await;
        self.iptables_delete(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_input",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await;
        self.iptables_delete(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_input",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-m".into(),
                "socket".into(),
                "--transparent".into(),
                "--nowildcard".into(),
                "-j".into(),
                "ACCEPT".into(),
            ],
        )
        .await;
        self.iptables_delete(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_input",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-p".into(),
                "icmpv6".into(),
                "-j".into(),
                "ACCEPT".into(),
            ],
        )
        .await;
        self.iptables_delete(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_forward",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await;
        self.iptables_delete(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_forward",
            &[
                "-i".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await;
        self.iptables_delete(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_output",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-p".into(),
                "icmpv6".into(),
                "--icmpv6-type".into(),
                "134".into(),
                "-j".into(),
                "REJECT".into(),
            ],
        )
        .await;
        self.iptables_delete(
            IP6TABLES,
            "filter",
            "vpnhotspot_v6_output",
            &[
                "-o".into(),
                config.downstream.clone(),
                "-p".into(),
                "icmpv6".into(),
                "--icmpv6-type".into(),
                "134".into(),
                "-m".into(),
                "mark".into(),
                "--mark".into(),
                format!("{DAEMON_REPLY_MARK}/{DAEMON_REPLY_MARK_MASK}"),
                "-j".into(),
                "ACCEPT".into(),
            ],
        )
        .await;
        delete_rule(IpRuleCommand {
            operation: IpOperation::Delete,
            family: IpFamily::Ipv6,
            iif: config.downstream.clone(),
            priority: rule_priority(RULE_PRIORITY_DAEMON_BASE),
            action: RuleAction::Lookup,
            table: DAEMON_TABLE,
            fwmark: Some((DAEMON_INTERCEPT_FWMARK_VALUE, DAEMON_INTERCEPT_FWMARK_MASK)),
        })
        .await;
        delete_route(IpRouteCommand {
            operation: IpOperation::Delete,
            route_type: RouteType::Local,
            destination: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            prefix_len: 0,
            interface: "lo".to_string(),
            table: DAEMON_TABLE,
        })
        .await;
        delete_route(IpRouteCommand {
            operation: IpOperation::Delete,
            route_type: RouteType::Unicast,
            destination: IpAddr::V6(route_address(ipv6_nat.gateway, ipv6_nat.prefix_len)),
            prefix_len: ipv6_nat.prefix_len,
            interface: config.downstream.clone(),
            table: LOCAL_NETWORK_TABLE,
        })
        .await;
        delete_address(IpAddressCommand {
            operation: IpOperation::Delete,
            address: IpAddr::V6(ipv6_nat.gateway),
            prefix_len: ipv6_nat.prefix_len,
            interface: config.downstream.clone(),
        })
        .await;
    }

    async fn iptables_new_chain(&self, binary: &str, table: &str, chain: &str) {
        let _ = run_command_status(binary, &["-w", "-t", table, "-N", chain]).await;
    }

    async fn iptables_insert(
        &self,
        binary: &str,
        table: &str,
        chain: &str,
        args: &[String],
    ) -> io::Result<()> {
        let mut command_args = vec![
            "-w".to_string(),
            "-t".to_string(),
            table.to_string(),
            "-I".to_string(),
            chain.to_string(),
        ];
        command_args.extend(args.iter().cloned());
        run_command(binary, &command_args).await
    }

    async fn iptables_delete(&self, binary: &str, table: &str, chain: &str, args: &[String]) {
        let mut command_args = vec![
            "-w".to_string(),
            "-t".to_string(),
            table.to_string(),
            "-D".to_string(),
            chain.to_string(),
        ];
        command_args.extend(args.iter().cloned());
        let _ = run_command_status_owned(binary, &command_args).await;
    }
}

pub(crate) async fn clean(command: &CleanIpCommand) -> io::Result<()> {
    delete_rule_repeated(IpFamily::Ipv6, rule_priority(RULE_PRIORITY_DAEMON_BASE)).await?;
    delete_rule_repeated(IpFamily::Ipv4, rule_priority(RULE_PRIORITY_UPSTREAM_BASE)).await?;
    delete_rule_repeated(
        IpFamily::Ipv4,
        rule_priority(RULE_PRIORITY_UPSTREAM_FALLBACK_BASE),
    )
    .await?;
    delete_rule_repeated(
        IpFamily::Ipv4,
        rule_priority(RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE),
    )
    .await?;
    rtnetlink::clean_ip(command).await?;
    clean_firewall().await;
    Ok(())
}

async fn clean_firewall() {
    delete_iptables_repeated(
        IPTABLES,
        "mangle",
        "PREROUTING",
        &["-j", "vpnhotspot_dns_tproxy"],
    )
    .await;
    delete_iptables_repeated(IPTABLES, "filter", "FORWARD", &["-j", "vpnhotspot_acl"]).await;
    delete_iptables_repeated(
        IPTABLES,
        "nat",
        "POSTROUTING",
        &["-j", "vpnhotspot_masquerade"],
    )
    .await;
    let _ = run_restore(
        IPTABLES_RESTORE,
        "*mangle
:vpnhotspot_dns_tproxy - [0:0]
-X vpnhotspot_dns_tproxy
COMMIT
*filter
:vpnhotspot_acl - [0:0]
:vpnhotspot_stats - [0:0]
-X vpnhotspot_acl
-X vpnhotspot_stats
COMMIT
*nat
-F PREROUTING
:vpnhotspot_masquerade - [0:0]
-X vpnhotspot_masquerade
COMMIT
",
    )
    .await;
    for chain in ["INPUT", "FORWARD", "OUTPUT"] {
        delete_iptables_repeated(IP6TABLES, "filter", chain, &["-j", "vpnhotspot_filter"]).await;
    }
    for (chain, target) in [
        ("INPUT", "vpnhotspot_v6_input"),
        ("FORWARD", "vpnhotspot_v6_forward"),
        ("OUTPUT", "vpnhotspot_v6_output"),
    ] {
        delete_iptables_repeated(IP6TABLES, "filter", chain, &["-j", target]).await;
    }
    delete_iptables_repeated(
        IP6TABLES,
        "mangle",
        "PREROUTING",
        &["-j", "vpnhotspot_v6_tproxy"],
    )
    .await;
    let _ = run_restore(
        IP6TABLES_RESTORE,
        "*filter
:vpnhotspot_filter - [0:0]
:vpnhotspot_v6_input - [0:0]
:vpnhotspot_v6_forward - [0:0]
:vpnhotspot_v6_output - [0:0]
-X vpnhotspot_filter
-X vpnhotspot_v6_input
-X vpnhotspot_v6_forward
-X vpnhotspot_v6_output
COMMIT
*mangle
:vpnhotspot_acl - [0:0]
:vpnhotspot_v6_tproxy - [0:0]
-X vpnhotspot_acl
-X vpnhotspot_v6_tproxy
COMMIT
",
    )
    .await;
}

async fn add_rule(command: IpRuleCommand) -> io::Result<()> {
    match rtnetlink::apply_rule(&command).await {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        result => result,
    }
}

async fn delete_rule(command: IpRuleCommand) {
    if let Err(e) = rtnetlink::apply_rule(&command).await {
        if !is_missing(&e) {
            eprintln!("delete rule failed: {e}");
        }
    }
}

async fn delete_rule_repeated(family: IpFamily, priority: u32) -> io::Result<()> {
    loop {
        let result = rtnetlink::apply_rule(&IpRuleCommand {
            operation: IpOperation::Delete,
            family,
            iif: String::new(),
            priority,
            action: RuleAction::Any,
            table: 0,
            fwmark: None,
        })
        .await;
        match result {
            Ok(()) => {}
            Err(e) if is_missing(&e) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

async fn apply_route(command: IpRouteCommand) -> io::Result<()> {
    match rtnetlink::apply_route(&command).await {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        result => result,
    }
}

async fn delete_route(command: IpRouteCommand) {
    if let Err(e) = rtnetlink::apply_route(&command).await {
        if !is_missing(&e) {
            eprintln!("delete route failed: {e}");
        }
    }
}

async fn apply_address(command: IpAddressCommand) -> io::Result<()> {
    match rtnetlink::apply_address(&command).await {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        result => result,
    }
}

async fn delete_address(command: IpAddressCommand) {
    if let Err(e) = rtnetlink::apply_address(&command).await {
        if !is_missing(&e) {
            eprintln!("delete address failed: {e}");
        }
    }
}

fn is_missing(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOENT | libc::ESRCH | libc::ENODEV)
    )
}

async fn delete_iptables_repeated(binary: &str, table: &str, chain: &str, args: &[&str]) {
    let mut command_args = vec!["-w", "-t", table, "-D", chain];
    command_args.extend_from_slice(args);
    while run_command_status(binary, &command_args)
        .await
        .unwrap_or(false)
    {}
}

async fn run_ndc(name: &str, args: &[&str]) -> io::Result<()> {
    let output = Command::new("ndc").args(args).output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let suffix = format!("200 0 {name} operation succeeded\n");
    if output.status.success() && stdout.ends_with(&suffix) {
        if stdout.len() > suffix.len() {
            eprintln!("ndc {}: {}", args.join(" "), stdout.trim_end());
        }
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ndc {} exited with {} stdout={} stderr={}",
            args.join(" "),
            output.status,
            stdout.trim_end(),
            String::from_utf8_lossy(&output.stderr).trim_end()
        )))
    }
}

async fn run_restore(binary: &str, input: &str) -> io::Result<()> {
    let mut child = Command::new(binary)
        .args(["-w", "--noflush"])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing restore stdin"))?
        .write_all(input.as_bytes())
        .await?;
    let status = child.wait().await?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{binary} exited with {status}")))
    }
}

async fn run_command(binary: &str, args: &[String]) -> io::Result<()> {
    let output = Command::new(binary).args(args).output().await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} {} exited with {} stdout={} stderr={}",
            binary,
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout).trim_end(),
            String::from_utf8_lossy(&output.stderr).trim_end()
        )))
    }
}

async fn run_command_status(binary: &str, args: &[&str]) -> io::Result<bool> {
    Ok(Command::new(binary)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?
        .success())
}

async fn run_command_status_owned(binary: &str, args: &[String]) -> io::Result<bool> {
    Ok(Command::new(binary)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?
        .success())
}

fn host_subnet(config: &SessionConfig) -> String {
    let raw = u32::from(config.dns_bind_address);
    let mask = if config.downstream_prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - config.downstream_prefix_len)
    };
    format!(
        "{}/{}",
        Ipv4Addr::from(raw & mask),
        config.downstream_prefix_len
    )
}

fn route_address(address: Ipv6Addr, prefix_len: u8) -> Ipv6Addr {
    let shift = 128u32.saturating_sub(prefix_len as u32);
    Ipv6Addr::from((u128::from(address) & (!0u128 << shift)).to_be_bytes())
}

fn mac_string(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

fn client_macs(config: &SessionConfig) -> HashSet<[u8; 6]> {
    config.clients.iter().map(|client| client.mac).collect()
}

fn client_ips(config: &SessionConfig) -> HashSet<Ipv4Addr> {
    config
        .clients
        .iter()
        .flat_map(|client| client.ipv4.iter().copied())
        .collect()
}

fn rule_priority(base: u32) -> u32 {
    (base as i32 + if android_api_level() < 31 { -3000 } else { 0 }) as u32
}

#[cfg(target_os = "android")]
fn android_api_level() -> i32 {
    extern "C" {
        fn android_get_device_api_level() -> libc::c_int;
    }
    unsafe { android_get_device_api_level() as i32 }
}

#[cfg(not(target_os = "android"))]
fn android_api_level() -> i32 {
    31
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_subnet_masks_host_bits() {
        let config = SessionConfig {
            downstream: "wlan0".to_string(),
            dns_bind_address: Ipv4Addr::new(192, 168, 43, 1),
            downstream_prefix_len: 24,
            reply_mark: DAEMON_REPLY_MARK,
            ip_forward: false,
            forward: true,
            masquerade: MasqueradeMode::Simple,
            ipv6_block: false,
            primary_network: None,
            primary_routes: Vec::new(),
            fallback_network: None,
            upstreams: Vec::new(),
            clients: Vec::new(),
            ipv6_nat: None,
        };
        assert_eq!(host_subnet(&config), "192.168.43.0/24");
    }

    #[test]
    fn mac_string_uses_xtables_format() {
        assert_eq!(
            mac_string(&[0x02, 0xab, 0, 0x7f, 0x80, 0xff]),
            "02:ab:00:7f:80:ff"
        );
    }
}
