use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::process::Stdio;

use futures_util::{pin_mut, TryStreamExt};
use rtnetlink::packet_route::{
    address::{AddressAttribute, AddressMessage},
    route::{
        RouteAttribute, RouteHeader, RouteMessage, RouteProtocol, RouteScope,
        RouteType as NetlinkRouteType,
    },
    rule::{RuleAction as NetlinkRuleAction, RuleAttribute, RuleMessage},
    AddressFamily,
};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{netlink, report};
use vpnhotspotd::shared::model::{
    ipv6_nat_gateway, ipv6_nat_prefix, ClientConfig, Ipv6NatConfig, Ipv6NatPorts, MasqueradeMode,
    SessionConfig, SessionPorts, UpstreamConfig, UpstreamRole, DAEMON_INTERCEPT_FWMARK_MASK,
    DAEMON_INTERCEPT_FWMARK_VALUE, DAEMON_REPLY_MARK, DAEMON_REPLY_MARK_MASK, DAEMON_TABLE,
    LOCAL_NETWORK_TABLE,
};
use vpnhotspotd::shared::protocol::{
    error_errno, CleanIpCommand, IoErrorReportExt, IoResultReportExt, IpAddressCommand, IpFamily,
    IpOperation, IpRouteCommand, IpRuleCommand, RouteType, RuleAction,
};

const IPTABLES: &str = "iptables";
const IP6TABLES: &str = "ip6tables";
const IPTABLES_RESTORE: &str = "iptables-restore";
const IP6TABLES_RESTORE: &str = "ip6tables-restore";

const RULE_PRIORITY_DAEMON_BASE: u32 = 20600;
const RULE_PRIORITY_UPSTREAM_BASE: u32 = 20700;
const RULE_PRIORITY_UPSTREAM_FALLBACK_BASE: u32 = 20800;
const RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE: u32 = 20900;

struct IptablesRule {
    binary: &'static str,
    table: &'static str,
    chain: &'static str,
    args: Vec<String>,
}

impl IptablesRule {
    fn new(
        binary: &'static str,
        table: &'static str,
        chain: &'static str,
        args: Vec<String>,
    ) -> Self {
        Self {
            binary,
            table,
            chain,
            args,
        }
    }

    async fn insert(&self) -> io::Result<()> {
        let mut command_args = vec![
            "-w".to_string(),
            "-t".to_string(),
            self.table.to_string(),
            "-I".to_string(),
            self.chain.to_string(),
        ];
        command_args.extend(self.args.iter().cloned());
        run_command(self.binary, &command_args).await
    }

    async fn delete(&self) {
        let mut command_args = vec![
            "-w".to_string(),
            "-t".to_string(),
            self.table.to_string(),
            "-D".to_string(),
            self.chain.to_string(),
        ];
        command_args.extend(self.args.iter().cloned());
        if let Err(e) = run_command_status_owned(self.binary, &command_args).await {
            report::io_with_details(
                "routing.iptables_delete",
                e,
                command_details(self.binary, command_args.iter().map(String::as_str)),
            );
        }
    }
}

pub(crate) struct Runtime {
    ports: SessionPorts,
    netlink: netlink::Handle,
}

impl Runtime {
    pub(crate) async fn start(
        config: &SessionConfig,
        ports: SessionPorts,
        netlink: netlink::Handle,
    ) -> io::Result<Self> {
        let runtime = Self { ports, netlink };
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
        let ipv6_nat_routing_changed = match (&previous.ipv6_nat, &next.ipv6_nat) {
            (Some(previous), Some(next)) => {
                previous.gateway != next.gateway || previous.prefix_len != next.prefix_len
            }
            (None, None) => false,
            _ => true,
        };
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
        if ipv6_nat_routing_changed {
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
        self.iptables_insert_rules(self.dns_rules(config)).await
    }

    async fn remove_dns(&self, config: &SessionConfig) {
        self.iptables_delete_rules(self.dns_rules(config)).await;
    }

    fn dns_rules(&self, config: &SessionConfig) -> Vec<IptablesRule> {
        [("tcp", self.ports.dns_tcp), ("udp", self.ports.dns_udp)]
            .into_iter()
            .map(|(protocol, port)| {
                IptablesRule::new(
                    IPTABLES,
                    "nat",
                    "PREROUTING",
                    vec![
                        "-i".into(),
                        config.downstream.clone(),
                        "-p".into(),
                        protocol.into(),
                        "-d".into(),
                        config.dns_bind_address.to_string(),
                        "--dport".into(),
                        "53".into(),
                        "-j".into(),
                        "DNAT".into(),
                        "--to-destination".into(),
                        format!(":{port}"),
                    ],
                )
            })
            .collect()
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
        if let Err(e) = run_ndc(
            "ipfwd",
            &[
                "ipfwd",
                "disable",
                &format!("vpnhotspot_{}", config.downstream),
            ],
        )
        .await
        {
            report::io_with_details(
                "routing.remove_ip_forward",
                e,
                [("downstream", config.downstream.clone())],
            );
        }
    }

    async fn add_disable_system_rule(&self, config: &SessionConfig) -> io::Result<()> {
        add_rule(
            &self.netlink,
            IpRuleCommand {
                operation: IpOperation::Replace,
                family: IpFamily::Ipv4,
                iif: config.downstream.clone(),
                priority: rule_priority(RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE),
                action: RuleAction::Unreachable,
                table: 0,
                fwmark: None,
            },
        )
        .await
    }

    async fn remove_disable_system_rule(&self, config: &SessionConfig) {
        delete_rule(
            &self.netlink,
            IpRuleCommand {
                operation: IpOperation::Delete,
                family: IpFamily::Ipv4,
                iif: config.downstream.clone(),
                priority: rule_priority(RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE),
                action: RuleAction::Unreachable,
                table: 0,
                fwmark: None,
            },
        )
        .await;
    }

    async fn add_forward(&self, config: &SessionConfig) -> io::Result<()> {
        self.iptables_new_chain(IPTABLES, "filter", "vpnhotspot_acl")
            .await;
        self.iptables_new_chain(IPTABLES, "filter", "vpnhotspot_stats")
            .await;
        self.iptables_insert_rules(self.forward_rules(config)).await
    }

    async fn remove_forward(&self, config: &SessionConfig) {
        self.iptables_delete_rules(self.forward_rules(config)).await;
    }

    fn forward_rules(&self, config: &SessionConfig) -> Vec<IptablesRule> {
        vec![
            IptablesRule::new(
                IPTABLES,
                "filter",
                "FORWARD",
                vec!["-j".into(), "vpnhotspot_acl".into()],
            ),
            IptablesRule::new(
                IPTABLES,
                "filter",
                "vpnhotspot_acl",
                vec![
                    "-i".into(),
                    config.downstream.clone(),
                    "!".into(),
                    "-o".into(),
                    config.downstream.clone(),
                    "-j".into(),
                    "REJECT".into(),
                ],
            ),
            IptablesRule::new(
                IPTABLES,
                "filter",
                "vpnhotspot_acl",
                vec![
                    "-o".into(),
                    config.downstream.clone(),
                    "-m".into(),
                    "state".into(),
                    "--state".into(),
                    "ESTABLISHED,RELATED".into(),
                    "-j".into(),
                    "ACCEPT".into(),
                ],
            ),
            IptablesRule::new(
                IPTABLES,
                "filter",
                "vpnhotspot_acl",
                vec![
                    "-o".into(),
                    config.downstream.clone(),
                    "-m".into(),
                    "state".into(),
                    "--state".into(),
                    "ESTABLISHED,RELATED".into(),
                    "-j".into(),
                    "vpnhotspot_stats".into(),
                ],
            ),
        ]
    }

    async fn add_masquerade_chain(&self, _config: &SessionConfig) -> io::Result<()> {
        self.iptables_new_chain(IPTABLES, "nat", "vpnhotspot_masquerade")
            .await;
        self.iptables_insert_rules([Self::masquerade_chain_rule()])
            .await
    }

    async fn remove_masquerade_chain(&self, _config: &SessionConfig) {
        self.iptables_delete_rules([Self::masquerade_chain_rule()])
            .await;
    }

    fn masquerade_chain_rule() -> IptablesRule {
        IptablesRule::new(
            IPTABLES,
            "nat",
            "POSTROUTING",
            vec!["-j".into(), "vpnhotspot_masquerade".into()],
        )
    }

    async fn add_upstream(
        &self,
        config: &SessionConfig,
        upstream: &UpstreamConfig,
    ) -> io::Result<()> {
        add_rule(
            &self.netlink,
            IpRuleCommand {
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
            },
        )
        .await?;
        self.add_upstream_masquerade(config, upstream).await
    }

    async fn remove_upstream(&self, config: &SessionConfig, upstream: &UpstreamConfig) {
        self.remove_upstream_masquerade(config, upstream).await;
        delete_rule(
            &self.netlink,
            IpRuleCommand {
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
            },
        )
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
                self.iptables_insert_rules([Self::upstream_masquerade_rule(config, upstream)])
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
                self.iptables_delete_rules([Self::upstream_masquerade_rule(config, upstream)])
                    .await;
            }
            // netd NAT is shared by interface pair, not owned by this session.
            MasqueradeMode::Netd => {}
        }
    }

    fn upstream_masquerade_rule(config: &SessionConfig, upstream: &UpstreamConfig) -> IptablesRule {
        IptablesRule::new(
            IPTABLES,
            "nat",
            "vpnhotspot_masquerade",
            vec![
                "-s".into(),
                host_subnet(config),
                "-o".into(),
                upstream.ifname.clone(),
                "-j".into(),
                "MASQUERADE".into(),
            ],
        )
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
        self.iptables_insert_rules(Self::client_mac_v4_rules(config, client))
            .await
    }

    async fn remove_client_mac_v4(&self, config: &SessionConfig, client: &ClientConfig) {
        let rules = Self::client_mac_v4_rules(config, client);
        for rule in rules.iter().rev() {
            rule.delete().await;
        }
    }

    fn client_mac_v4_rules(config: &SessionConfig, client: &ClientConfig) -> Vec<IptablesRule> {
        let mac = mac_string(&client.mac);
        vec![
            IptablesRule::new(
                IPTABLES,
                "filter",
                "vpnhotspot_acl",
                vec![
                    "-i".into(),
                    config.downstream.clone(),
                    "-m".into(),
                    "mac".into(),
                    "--mac-source".into(),
                    mac.clone(),
                    "-j".into(),
                    "ACCEPT".into(),
                ],
            ),
            IptablesRule::new(
                IPTABLES,
                "filter",
                "vpnhotspot_acl",
                vec![
                    "-i".into(),
                    config.downstream.clone(),
                    "-m".into(),
                    "mac".into(),
                    "--mac-source".into(),
                    mac,
                    "-j".into(),
                    "vpnhotspot_stats".into(),
                ],
            ),
        ]
    }

    async fn add_client_ip_stats(
        &self,
        config: &SessionConfig,
        address: Ipv4Addr,
    ) -> io::Result<()> {
        self.iptables_insert_rules(Self::client_ip_stats_rules(config, address))
            .await
    }

    async fn remove_client_ip_stats(&self, config: &SessionConfig, address: Ipv4Addr) {
        self.iptables_delete_rules(Self::client_ip_stats_rules(config, address))
            .await;
    }

    fn client_ip_stats_rules(config: &SessionConfig, address: Ipv4Addr) -> Vec<IptablesRule> {
        let address = address.to_string();
        vec![
            IptablesRule::new(
                IPTABLES,
                "filter",
                "vpnhotspot_stats",
                vec![
                    "-i".into(),
                    config.downstream.clone(),
                    "-s".into(),
                    address.clone(),
                    "-j".into(),
                    "RETURN".into(),
                ],
            ),
            IptablesRule::new(
                IPTABLES,
                "filter",
                "vpnhotspot_stats",
                vec![
                    "-o".into(),
                    config.downstream.clone(),
                    "-d".into(),
                    address,
                    "-j".into(),
                    "RETURN".into(),
                ],
            ),
        ]
    }

    async fn add_client_mac_v6(&self, client: &ClientConfig) -> io::Result<()> {
        self.iptables_insert_rules([Self::client_mac_v6_rule(client)])
            .await
    }

    async fn remove_client_mac_v6(&self, client: &ClientConfig) {
        self.iptables_delete_rules([Self::client_mac_v6_rule(client)])
            .await;
    }

    fn client_mac_v6_rule(client: &ClientConfig) -> IptablesRule {
        IptablesRule::new(
            IP6TABLES,
            "mangle",
            "vpnhotspot_acl",
            vec![
                "-m".into(),
                "mac".into(),
                "--mac-source".into(),
                mac_string(&client.mac),
                "-j".into(),
                "RETURN".into(),
            ],
        )
    }

    async fn add_ipv6_block(&self, config: &SessionConfig) -> io::Result<()> {
        self.iptables_new_chain(IP6TABLES, "filter", "vpnhotspot_filter")
            .await;
        self.iptables_insert_rules(Self::ipv6_block_rules(config))
            .await
    }

    async fn remove_ipv6_block(&self, config: &SessionConfig) {
        self.iptables_delete_rules(Self::ipv6_block_rules(config))
            .await;
    }

    fn ipv6_block_rules(config: &SessionConfig) -> Vec<IptablesRule> {
        let mut rules = Vec::new();
        for chain in ["INPUT", "FORWARD", "OUTPUT"] {
            rules.push(IptablesRule::new(
                IP6TABLES,
                "filter",
                chain,
                vec!["-j".into(), "vpnhotspot_filter".into()],
            ));
        }
        rules.push(IptablesRule::new(
            IP6TABLES,
            "filter",
            "vpnhotspot_filter",
            vec![
                "-i".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        ));
        rules.push(IptablesRule::new(
            IP6TABLES,
            "filter",
            "vpnhotspot_filter",
            vec![
                "-o".into(),
                config.downstream.clone(),
                "-j".into(),
                "REJECT".into(),
            ],
        ));
        rules
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
        apply_route(
            &self.netlink,
            IpRouteCommand {
                operation: IpOperation::Replace,
                route_type: RouteType::Unicast,
                destination: IpAddr::V6(route_address(ipv6_nat.gateway, ipv6_nat.prefix_len)),
                prefix_len: ipv6_nat.prefix_len,
                interface: config.downstream.clone(),
                table: LOCAL_NETWORK_TABLE,
            },
        )
        .await?;
        apply_address(
            &self.netlink,
            IpAddressCommand {
                operation: IpOperation::Replace,
                address: IpAddr::V6(ipv6_nat.gateway),
                prefix_len: ipv6_nat.prefix_len,
                interface: config.downstream.clone(),
            },
        )
        .await?;
        apply_route(
            &self.netlink,
            IpRouteCommand {
                operation: IpOperation::Replace,
                route_type: RouteType::Local,
                destination: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                prefix_len: 0,
                interface: "lo".to_string(),
                table: DAEMON_TABLE,
            },
        )
        .await?;
        add_rule(
            &self.netlink,
            IpRuleCommand {
                operation: IpOperation::Replace,
                family: IpFamily::Ipv6,
                iif: config.downstream.clone(),
                priority: rule_priority(RULE_PRIORITY_DAEMON_BASE),
                action: RuleAction::Lookup,
                table: DAEMON_TABLE,
                fwmark: Some((DAEMON_INTERCEPT_FWMARK_VALUE, DAEMON_INTERCEPT_FWMARK_MASK)),
            },
        )
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
        self.iptables_insert_rules(Self::ipv6_nat_filter_rules(config))
            .await?;
        self.iptables_insert_rules([Self::ipv6_nat_acl_drop_rule()])
            .await?;
        self.iptables_insert_rules(Self::ipv6_nat_tproxy_port_rules(config, ports))
            .await?;
        self.iptables_insert_rules([Self::ipv6_nat_tproxy_acl_rule(config)])
            .await?;
        self.iptables_insert_rules(Self::ipv6_nat_filter_jump_rules())
            .await?;
        self.iptables_insert_rules([Self::ipv6_nat_prerouting_rule()])
            .await
    }

    async fn remove_ipv6_nat(&self, config: &SessionConfig, ipv6_nat: &Ipv6NatConfig) {
        if let Some(ports) = self.ports.ipv6_nat {
            self.iptables_delete_rules(Self::ipv6_nat_tproxy_port_rules(config, ports))
                .await;
        }
        self.iptables_delete_rules([Self::ipv6_nat_tproxy_acl_rule(config)])
            .await;
        self.iptables_delete_rules([Self::ipv6_nat_acl_drop_rule()])
            .await;
        self.iptables_delete_rules(Self::ipv6_nat_filter_jump_rules())
            .await;
        self.iptables_delete_rules([Self::ipv6_nat_prerouting_rule()])
            .await;
        self.iptables_delete_rules(Self::ipv6_nat_filter_rules(config))
            .await;
        delete_rule(
            &self.netlink,
            IpRuleCommand {
                operation: IpOperation::Delete,
                family: IpFamily::Ipv6,
                iif: config.downstream.clone(),
                priority: rule_priority(RULE_PRIORITY_DAEMON_BASE),
                action: RuleAction::Lookup,
                table: DAEMON_TABLE,
                fwmark: Some((DAEMON_INTERCEPT_FWMARK_VALUE, DAEMON_INTERCEPT_FWMARK_MASK)),
            },
        )
        .await;
        delete_route(
            &self.netlink,
            IpRouteCommand {
                operation: IpOperation::Delete,
                route_type: RouteType::Local,
                destination: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                prefix_len: 0,
                interface: "lo".to_string(),
                table: DAEMON_TABLE,
            },
        )
        .await;
        delete_route(
            &self.netlink,
            IpRouteCommand {
                operation: IpOperation::Delete,
                route_type: RouteType::Unicast,
                destination: IpAddr::V6(route_address(ipv6_nat.gateway, ipv6_nat.prefix_len)),
                prefix_len: ipv6_nat.prefix_len,
                interface: config.downstream.clone(),
                table: LOCAL_NETWORK_TABLE,
            },
        )
        .await;
        delete_address(
            &self.netlink,
            IpAddressCommand {
                operation: IpOperation::Delete,
                address: IpAddr::V6(ipv6_nat.gateway),
                prefix_len: ipv6_nat.prefix_len,
                interface: config.downstream.clone(),
            },
        )
        .await;
    }

    fn ipv6_nat_filter_rules(config: &SessionConfig) -> Vec<IptablesRule> {
        vec![
            IptablesRule::new(
                IP6TABLES,
                "filter",
                "vpnhotspot_v6_input",
                vec![
                    "-i".into(),
                    config.downstream.clone(),
                    "-j".into(),
                    "REJECT".into(),
                ],
            ),
            IptablesRule::new(
                IP6TABLES,
                "filter",
                "vpnhotspot_v6_input",
                vec![
                    "-i".into(),
                    config.downstream.clone(),
                    "-m".into(),
                    "socket".into(),
                    "--transparent".into(),
                    "--nowildcard".into(),
                    "-j".into(),
                    "ACCEPT".into(),
                ],
            ),
            IptablesRule::new(
                IP6TABLES,
                "filter",
                "vpnhotspot_v6_input",
                vec![
                    "-i".into(),
                    config.downstream.clone(),
                    "-p".into(),
                    "icmpv6".into(),
                    "-j".into(),
                    "ACCEPT".into(),
                ],
            ),
            IptablesRule::new(
                IP6TABLES,
                "filter",
                "vpnhotspot_v6_forward",
                vec![
                    "-o".into(),
                    config.downstream.clone(),
                    "-j".into(),
                    "REJECT".into(),
                ],
            ),
            IptablesRule::new(
                IP6TABLES,
                "filter",
                "vpnhotspot_v6_forward",
                vec![
                    "-i".into(),
                    config.downstream.clone(),
                    "-j".into(),
                    "REJECT".into(),
                ],
            ),
            IptablesRule::new(
                IP6TABLES,
                "filter",
                "vpnhotspot_v6_output",
                vec![
                    "-o".into(),
                    config.downstream.clone(),
                    "-p".into(),
                    "icmpv6".into(),
                    "--icmpv6-type".into(),
                    "134".into(),
                    "-j".into(),
                    "REJECT".into(),
                ],
            ),
            IptablesRule::new(
                IP6TABLES,
                "filter",
                "vpnhotspot_v6_output",
                vec![
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
            ),
        ]
    }

    fn ipv6_nat_acl_drop_rule() -> IptablesRule {
        IptablesRule::new(
            IP6TABLES,
            "mangle",
            "vpnhotspot_acl",
            vec!["-j".into(), "DROP".into()],
        )
    }

    fn ipv6_nat_tproxy_port_rules(
        config: &SessionConfig,
        ports: Ipv6NatPorts,
    ) -> Vec<IptablesRule> {
        [("tcp", ports.tcp), ("udp", ports.udp)]
            .into_iter()
            .map(|(protocol, port)| {
                IptablesRule::new(
                    IP6TABLES,
                    "mangle",
                    "vpnhotspot_v6_tproxy",
                    vec![
                        "-i".into(),
                        config.downstream.clone(),
                        "-p".into(),
                        protocol.into(),
                        "-j".into(),
                        "TPROXY".into(),
                        "--on-port".into(),
                        port.to_string(),
                        "--tproxy-mark".into(),
                        "0x10000000/0x10000000".into(),
                    ],
                )
            })
            .collect()
    }

    fn ipv6_nat_tproxy_acl_rule(config: &SessionConfig) -> IptablesRule {
        IptablesRule::new(
            IP6TABLES,
            "mangle",
            "vpnhotspot_v6_tproxy",
            vec![
                "-i".into(),
                config.downstream.clone(),
                "-j".into(),
                "vpnhotspot_acl".into(),
            ],
        )
    }

    fn ipv6_nat_filter_jump_rules() -> Vec<IptablesRule> {
        [
            ("INPUT", "vpnhotspot_v6_input"),
            ("FORWARD", "vpnhotspot_v6_forward"),
            ("OUTPUT", "vpnhotspot_v6_output"),
        ]
        .into_iter()
        .map(|(chain, target)| {
            IptablesRule::new(IP6TABLES, "filter", chain, vec!["-j".into(), target.into()])
        })
        .collect()
    }

    fn ipv6_nat_prerouting_rule() -> IptablesRule {
        IptablesRule::new(
            IP6TABLES,
            "mangle",
            "PREROUTING",
            vec!["-j".into(), "vpnhotspot_v6_tproxy".into()],
        )
    }

    async fn iptables_new_chain(&self, binary: &str, table: &str, chain: &str) {
        let args = ["-w", "-t", table, "-N", chain];
        if let Err(e) = run_command_status(binary, &args).await {
            report::io_with_details(
                "routing.iptables_new_chain",
                e,
                command_details(binary, args),
            );
        }
    }

    async fn iptables_insert_rules<I>(&self, rules: I) -> io::Result<()>
    where
        I: IntoIterator<Item = IptablesRule>,
    {
        for rule in rules {
            rule.insert().await?;
        }
        Ok(())
    }

    async fn iptables_delete_rules<I>(&self, rules: I)
    where
        I: IntoIterator<Item = IptablesRule>,
    {
        for rule in rules {
            rule.delete().await;
        }
    }
}

pub(crate) async fn clean(handle: &netlink::Handle, command: &CleanIpCommand) -> io::Result<()> {
    delete_rule_repeated(
        handle,
        IpFamily::Ipv6,
        rule_priority(RULE_PRIORITY_DAEMON_BASE),
    )
    .await?;
    delete_rule_repeated(
        handle,
        IpFamily::Ipv4,
        rule_priority(RULE_PRIORITY_UPSTREAM_BASE),
    )
    .await?;
    delete_rule_repeated(
        handle,
        IpFamily::Ipv4,
        rule_priority(RULE_PRIORITY_UPSTREAM_FALLBACK_BASE),
    )
    .await?;
    delete_rule_repeated(
        handle,
        IpFamily::Ipv4,
        rule_priority(RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE),
    )
    .await?;
    clean_ip(handle, command).await?;
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
    if let Err(e) = run_restore(
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
    .await
    {
        report::io("routing.clean_firewall.iptables_restore", e);
    }
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
    if let Err(e) = run_restore(
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
    .await
    {
        report::io("routing.clean_firewall.ip6tables_restore", e);
    }
}

pub(crate) async fn apply_static_address(
    handle: &netlink::Handle,
    command: &IpAddressCommand,
) -> io::Result<()> {
    apply_address_command(handle, command).await
}

async fn clean_ip(handle: &netlink::Handle, command: &CleanIpCommand) -> io::Result<()> {
    flush_routes(handle, AddressFamily::Inet6, DAEMON_TABLE).await?;
    for interface in netlink::link_names(handle).await?.into_values() {
        let prefix = ipv6_nat_prefix(&command.ipv6_nat_prefix_seed, &interface);
        let address = IpAddressCommand {
            operation: IpOperation::Delete,
            address: IpAddr::V6(ipv6_nat_gateway(prefix)),
            prefix_len: prefix.prefix_len,
            interface: interface.clone(),
        };
        if let Err(e) = apply_address_command(handle, &address).await {
            if !is_missing(&e) {
                report::io_with_details("routing.clean_ip.address", e, address_details(&address));
            }
        }
        let route = IpRouteCommand {
            operation: IpOperation::Delete,
            route_type: RouteType::Unicast,
            destination: IpAddr::V6(Ipv6Addr::from(prefix.prefix.to_be_bytes())),
            prefix_len: prefix.prefix_len,
            interface,
            table: LOCAL_NETWORK_TABLE,
        };
        if let Err(e) = apply_route_command(handle, &route).await {
            if !is_missing(&e) {
                report::io_with_details("routing.clean_ip.route", e, route_details(&route));
            }
        }
    }
    Ok(())
}

async fn add_rule(handle: &netlink::Handle, command: IpRuleCommand) -> io::Result<()> {
    match apply_rule_command(handle, &command).await {
        Err(e) if error_errno(&e) == Some(libc::EEXIST) => Ok(()),
        result => result,
    }
}

async fn delete_rule(handle: &netlink::Handle, command: IpRuleCommand) {
    if let Err(e) = apply_rule_command(handle, &command).await {
        if !is_missing(&e) {
            report::io_with_details("routing.delete_rule", e, rule_details(&command));
        }
    }
}

async fn delete_rule_repeated(
    handle: &netlink::Handle,
    family: IpFamily,
    priority: u32,
) -> io::Result<()> {
    loop {
        let result = apply_rule_command(
            handle,
            &IpRuleCommand {
                operation: IpOperation::Delete,
                family,
                iif: String::new(),
                priority,
                action: RuleAction::Any,
                table: 0,
                fwmark: None,
            },
        )
        .await;
        match result {
            Ok(()) => {}
            Err(e) if is_missing(&e) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

async fn apply_route(handle: &netlink::Handle, command: IpRouteCommand) -> io::Result<()> {
    match apply_route_command(handle, &command).await {
        Err(e) if error_errno(&e) == Some(libc::EEXIST) => Ok(()),
        result => result,
    }
}

async fn delete_route(handle: &netlink::Handle, command: IpRouteCommand) {
    if let Err(e) = apply_route_command(handle, &command).await {
        if !is_missing(&e) {
            report::io_with_details("routing.delete_route", e, route_details(&command));
        }
    }
}

async fn apply_address(handle: &netlink::Handle, command: IpAddressCommand) -> io::Result<()> {
    match apply_address_command(handle, &command).await {
        Err(e) if error_errno(&e) == Some(libc::EEXIST) => Ok(()),
        result => result,
    }
}

async fn delete_address(handle: &netlink::Handle, command: IpAddressCommand) {
    if let Err(e) = apply_address_command(handle, &command).await {
        if !is_missing(&e) {
            report::io_with_details("routing.delete_address", e, address_details(&command));
        }
    }
}

async fn apply_address_command(
    handle: &netlink::Handle,
    command: &IpAddressCommand,
) -> io::Result<()> {
    let index = netlink::link_index(handle, &command.interface)
        .await
        .with_report_context_details("routing.address.link_index", address_details(command))?;
    match command.operation {
        IpOperation::Replace => {
            let mut request = handle
                .raw()
                .address()
                .add(index, command.address, command.prefix_len)
                .replace();
            *request.message_mut() = address_message(index, command.address, command.prefix_len);
            request.execute().await
        }
        IpOperation::Delete => {
            handle
                .raw()
                .address()
                .del(address_message(index, command.address, command.prefix_len))
                .execute()
                .await
        }
    }
    .map_err(netlink::to_io_error)
    .with_report_context_details("routing.address", address_details(command))
}

async fn apply_route_command(handle: &netlink::Handle, command: &IpRouteCommand) -> io::Result<()> {
    let message = route_message(
        command,
        netlink::link_index(handle, &command.interface)
            .await
            .with_report_context_details("routing.route.link_index", route_details(command))?,
    );
    match command.operation {
        IpOperation::Replace => handle.raw().route().add(message).replace().execute().await,
        IpOperation::Delete => handle.raw().route().del(message).execute().await,
    }
    .map_err(netlink::to_io_error)
    .with_report_context_details("routing.route", route_details(command))
}

async fn apply_rule_command(handle: &netlink::Handle, command: &IpRuleCommand) -> io::Result<()> {
    match command.operation {
        IpOperation::Replace => {
            let mut request = handle.raw().rule().add();
            fill_rule_message(request.message_mut(), command)
                .with_report_context_details("routing.rule.fill", rule_details(command))?;
            request.execute().await
        }
        IpOperation::Delete => {
            handle
                .raw()
                .rule()
                .del(
                    rule_message(command)
                        .with_report_context_details("routing.rule.fill", rule_details(command))?,
                )
                .execute()
                .await
        }
    }
    .map_err(netlink::to_io_error)
    .with_report_context_details("routing.rule", rule_details(command))
}

async fn flush_routes(
    handle: &netlink::Handle,
    family: AddressFamily,
    table: u32,
) -> io::Result<()> {
    let _dump = handle.lock_dump().await;
    let routes = handle
        .raw()
        .route()
        .get(route_dump_message(family, table))
        .execute();
    pin_mut!(routes);
    while let Some(route) = routes.try_next().await.map_err(netlink::to_io_error)? {
        if route_table(&route) == Some(table) {
            if let Err(e) = handle.raw().route().del(route).execute().await {
                let e = netlink::to_io_error(e);
                if !is_missing(&e) {
                    report::io_with_details(
                        "routing.flush_routes.delete",
                        e,
                        [
                            ("family", format!("{family:?}")),
                            ("table", table.to_string()),
                        ],
                    );
                }
            }
        }
    }
    Ok(())
}

fn address_message(index: u32, address: IpAddr, prefix_len: u8) -> AddressMessage {
    let mut message = AddressMessage::default();
    message.header.family = address_family(&address);
    message.header.prefix_len = prefix_len;
    message.header.index = index;
    message.attributes.push(AddressAttribute::Local(address));
    message.attributes.push(AddressAttribute::Address(address));
    message
}

fn route_message(command: &IpRouteCommand, interface: u32) -> RouteMessage {
    let mut message = route_dump_message(address_family(&command.destination), command.table);
    message.header.destination_prefix_length = command.prefix_len;
    message.header.protocol = RouteProtocol::Static;
    match command.route_type {
        RouteType::Unicast => {
            message.header.kind = NetlinkRouteType::Unicast;
            message.header.scope = RouteScope::Link;
        }
        RouteType::Local => {
            message.header.kind = NetlinkRouteType::Local;
            message.header.scope = RouteScope::Host;
        }
    }
    if command.prefix_len > 0 {
        message
            .attributes
            .push(RouteAttribute::Destination(command.destination.into()));
    }
    message.attributes.push(RouteAttribute::Oif(interface));
    message
}

fn route_dump_message(family: AddressFamily, table: u32) -> RouteMessage {
    let mut message = RouteMessage::default();
    message.header.address_family = family;
    message.header.table = if table < 256 {
        table as u8
    } else {
        RouteHeader::RT_TABLE_UNSPEC
    };
    message.attributes.push(RouteAttribute::Table(table));
    message
}

fn rule_message(command: &IpRuleCommand) -> io::Result<RuleMessage> {
    let mut message = RuleMessage::default();
    fill_rule_message(&mut message, command)?;
    Ok(message)
}

fn fill_rule_message(message: &mut RuleMessage, command: &IpRuleCommand) -> io::Result<()> {
    if !command.iif.is_empty() {
        netlink::validate_interface_name(&command.iif)?;
    }
    message.header.family = family_value(command.family);
    message.header.dst_len = 0;
    message.header.src_len = 0;
    message.header.tos = 0;
    message.header.table = if command.table < 256 {
        command.table as u8
    } else {
        RouteHeader::RT_TABLE_UNSPEC
    };
    message.header.action = match command.action {
        RuleAction::Lookup => NetlinkRuleAction::ToTable,
        RuleAction::Unreachable => NetlinkRuleAction::Unreachable,
        RuleAction::Any => NetlinkRuleAction::Unspec,
    };
    message.attributes.clear();
    if !command.iif.is_empty() {
        message
            .attributes
            .push(RuleAttribute::Iifname(command.iif.clone()));
    }
    message
        .attributes
        .push(RuleAttribute::Priority(command.priority));
    if command.action == RuleAction::Lookup {
        message.attributes.push(RuleAttribute::Table(command.table));
    }
    if let Some((mark, mask)) = command.fwmark {
        message.attributes.push(RuleAttribute::FwMark(mark));
        message.attributes.push(RuleAttribute::FwMask(mask));
    }
    Ok(())
}

fn route_table(route: &RouteMessage) -> Option<u32> {
    route
        .attributes
        .iter()
        .find_map(|attribute| {
            if let RouteAttribute::Table(table) = attribute {
                Some(*table)
            } else {
                None
            }
        })
        .or({
            if route.header.table == RouteHeader::RT_TABLE_UNSPEC {
                None
            } else {
                Some(route.header.table as u32)
            }
        })
}

fn address_family(address: &IpAddr) -> AddressFamily {
    match address {
        IpAddr::V4(_) => AddressFamily::Inet,
        IpAddr::V6(_) => AddressFamily::Inet6,
    }
}

fn family_value(family: IpFamily) -> AddressFamily {
    match family {
        IpFamily::Ipv4 => AddressFamily::Inet,
        IpFamily::Ipv6 => AddressFamily::Inet6,
    }
}

fn is_missing(error: &io::Error) -> bool {
    matches!(
        error_errno(error),
        Some(libc::ENOENT | libc::ESRCH | libc::ENODEV)
    )
}

fn command_details<'a>(
    binary: &str,
    args: impl IntoIterator<Item = &'a str>,
) -> Vec<(String, String)> {
    let args = args.into_iter().collect::<Vec<_>>().join(" ");
    vec![
        ("binary".to_owned(), binary.to_owned()),
        ("args".to_owned(), args),
    ]
}

fn restore_details(binary: &str, input: &str) -> Vec<(String, String)> {
    vec![
        ("binary".to_owned(), binary.to_owned()),
        ("stdin".to_owned(), input.to_owned()),
    ]
}

fn rule_details(command: &IpRuleCommand) -> Vec<(String, String)> {
    vec![
        ("operation".to_owned(), format!("{:?}", command.operation)),
        ("family".to_owned(), format!("{:?}", command.family)),
        ("iif".to_owned(), command.iif.clone()),
        ("priority".to_owned(), command.priority.to_string()),
        ("action".to_owned(), format!("{:?}", command.action)),
        ("table".to_owned(), command.table.to_string()),
        (
            "fwmark".to_owned(),
            command
                .fwmark
                .map(|(mark, mask)| format!("{mark:#x}/{mask:#x}"))
                .unwrap_or_default(),
        ),
    ]
}

fn route_details(command: &IpRouteCommand) -> Vec<(String, String)> {
    vec![
        ("operation".to_owned(), format!("{:?}", command.operation)),
        ("type".to_owned(), format!("{:?}", command.route_type)),
        ("destination".to_owned(), command.destination.to_string()),
        ("prefix_len".to_owned(), command.prefix_len.to_string()),
        ("interface".to_owned(), command.interface.clone()),
        ("table".to_owned(), command.table.to_string()),
    ]
}

fn address_details(command: &IpAddressCommand) -> Vec<(String, String)> {
    vec![
        ("operation".to_owned(), format!("{:?}", command.operation)),
        ("address".to_owned(), command.address.to_string()),
        ("prefix_len".to_owned(), command.prefix_len.to_string()),
        ("interface".to_owned(), command.interface.clone()),
    ]
}

async fn delete_iptables_repeated(binary: &str, table: &str, chain: &str, args: &[&str]) {
    let mut command_args = vec!["-w", "-t", table, "-D", chain];
    command_args.extend_from_slice(args);
    loop {
        match run_command_status(binary, &command_args).await {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                report::io_with_details(
                    "routing.delete_iptables_repeated",
                    e,
                    command_details(binary, command_args.iter().copied()),
                );
                break;
            }
        }
    }
}

async fn run_ndc(name: &str, args: &[&str]) -> io::Result<()> {
    let output = Command::new("ndc")
        .args(args)
        .output()
        .await
        .with_report_context_details(
            "routing.ndc.spawn",
            command_details("ndc", args.iter().copied()),
        )?;
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
        .with_report_context_details(
            "routing.ndc.status",
            command_details("ndc", args.iter().copied()),
        )
    }
}

async fn run_restore(binary: &str, input: &str) -> io::Result<()> {
    let mut child = Command::new(binary)
        .args(["-w", "--noflush"])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_report_context_details("routing.restore.spawn", restore_details(binary, input))?;
    child
        .stdin
        .take()
        .ok_or_else(|| {
            io::Error::other("missing restore stdin").with_report_context_details(
                "routing.restore.stdin",
                restore_details(binary, input),
            )
        })?
        .write_all(input.as_bytes())
        .await
        .with_report_context_details("routing.restore.write", restore_details(binary, input))?;
    let status = child
        .wait()
        .await
        .with_report_context_details("routing.restore.wait", restore_details(binary, input))?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{binary} exited with {status}")))
            .with_report_context_details("routing.restore.status", restore_details(binary, input))
    }
}

async fn run_command(binary: &str, args: &[String]) -> io::Result<()> {
    let output = Command::new(binary)
        .args(args)
        .output()
        .await
        .with_report_context_details(
            "routing.command.spawn",
            command_details(binary, args.iter().map(String::as_str)),
        )?;
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
        .with_report_context_details(
            "routing.command.status",
            command_details(binary, args.iter().map(String::as_str)),
        )
    }
}

async fn run_command_status(binary: &str, args: &[&str]) -> io::Result<bool> {
    Ok(Command::new(binary)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .with_report_context_details(
            "routing.command_status.spawn",
            command_details(binary, args.iter().copied()),
        )?
        .success())
}

async fn run_command_status_owned(binary: &str, args: &[String]) -> io::Result<bool> {
    Ok(Command::new(binary)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .with_report_context_details(
            "routing.command_status.spawn",
            command_details(binary, args.iter().map(String::as_str)),
        )?
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
    rule_priority_for_api(base, android_api_level())
}

fn rule_priority_for_api(base: u32, api_level: i32) -> u32 {
    (base as i32 + if api_level < 31 { -3000 } else { 0 }) as u32
}

fn android_api_level() -> i32 {
    extern "C" {
        fn android_get_device_api_level() -> libc::c_int;
    }
    unsafe { android_get_device_api_level() as i32 }
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

    #[test]
    fn rule_priority_uses_android_routing_gap() {
        assert_eq!(rule_priority_for_api(RULE_PRIORITY_DAEMON_BASE, 30), 17600);
        assert_eq!(rule_priority_for_api(RULE_PRIORITY_DAEMON_BASE, 31), 20600);
    }
}
