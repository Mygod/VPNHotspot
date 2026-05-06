use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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
use tokio::process::Command;

use crate::{
    firewall::{self, IptablesTarget},
    netlink, report,
};
use vpnhotspotd::shared::model::{
    ipv6_nat_gateway, ipv6_nat_prefix, ClientConfig, Ipv6NatPorts, MasqueradeMode, SessionConfig,
    SessionPorts, UpstreamConfig, UpstreamRole, DAEMON_INTERCEPT_FWMARK_MASK,
    DAEMON_INTERCEPT_FWMARK_VALUE, DAEMON_REPLY_MARK, DAEMON_REPLY_MARK_MASK, DAEMON_TABLE,
    LOCAL_NETWORK_TABLE,
};
use vpnhotspotd::shared::protocol::{
    error_errno, CleanIpCommand, IoResultReportExt, IpAddressCommand, IpFamily, IpOperation,
    IpRouteCommand, IpRuleCommand, RouteType, RuleAction,
};

const RULE_PRIORITY_DAEMON_BASE: u32 = 20600;
const RULE_PRIORITY_UPSTREAM_BASE: u32 = 20700;
const RULE_PRIORITY_UPSTREAM_FALLBACK_BASE: u32 = 20800;
const RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE: u32 = 20900;

#[derive(Clone, Debug, Eq, PartialEq)]
struct IptablesRule {
    target: IptablesTarget,
    table: &'static str,
    chain: &'static str,
    args: Vec<String>,
}

impl IptablesRule {
    fn new(
        target: IptablesTarget,
        table: &'static str,
        chain: &'static str,
        args: Vec<String>,
    ) -> Self {
        Self {
            target,
            table,
            chain,
            args,
        }
    }

    fn insert_line(&self) -> io::Result<String> {
        firewall::restore_line("-I", self.chain, &self.args)
    }

    async fn delete(&self) -> bool {
        match self.delete_input() {
            Ok(input) => {
                if let Err(e) = firewall::restore_status(self.target, &input).await {
                    report::io_with_details("routing.iptables_delete", e, self.details());
                    false
                } else {
                    true
                }
            }
            Err(e) => {
                report::io_with_details("routing.iptables_delete", e, self.details());
                false
            }
        }
    }

    fn delete_input(&self) -> io::Result<String> {
        Ok(firewall::restore_input(
            self.table,
            &[firewall::restore_line("-D", self.chain, &self.args)?],
        ))
    }

    fn details(&self) -> Vec<(String, String)> {
        vec![
            ("binary".to_owned(), self.target.restore_binary().to_owned()),
            ("table".to_owned(), self.table.to_owned()),
            ("chain".to_owned(), self.chain.to_owned()),
            ("args".to_owned(), self.args.join(" ")),
        ]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RoutingMutation {
    EnsureIptablesChain {
        target: IptablesTarget,
        table: &'static str,
        chain: &'static str,
    },
    Iptables(IptablesRule),
    IpForward {
        downstream: String,
    },
    IpRule(IpRuleCommand),
    IpRoute(IpRouteCommand),
    IpAddress(IpAddressCommand),
    NetdNat {
        downstream: String,
        upstream: String,
    },
}

impl RoutingMutation {
    async fn apply(&self, handle: &netlink::Handle) -> io::Result<()> {
        match self {
            Self::EnsureIptablesChain {
                target,
                table,
                chain,
            } => {
                ensure_iptables_chain(*target, table, chain).await;
                Ok(())
            }
            Self::Iptables(rule) => {
                apply_iptables_batch(rule.target, rule.table, std::slice::from_ref(rule)).await
            }
            Self::IpForward { downstream } => add_ip_forward(downstream).await,
            Self::IpRule(command) => add_rule(handle, command.clone()).await,
            Self::IpRoute(command) => apply_route(handle, command.clone()).await,
            Self::IpAddress(command) => apply_address(handle, command.clone()).await,
            Self::NetdNat {
                downstream,
                upstream,
            } => run_ndc("Nat", &["nat", "enable", downstream, upstream, "0"]).await,
        }
    }

    async fn delete(&self, handle: &netlink::Handle) -> bool {
        match self {
            Self::EnsureIptablesChain { .. } => true,
            Self::Iptables(rule) => rule.delete().await,
            Self::IpForward { downstream } => remove_ip_forward(downstream).await,
            Self::IpRule(command) => delete_rule_result(handle, command.clone()).await,
            Self::IpRoute(command) => delete_route_result(handle, command.clone()).await,
            Self::IpAddress(command) => delete_address_result(handle, command.clone()).await,
            // netd NAT is shared by interface pair, not owned by this session.
            Self::NetdNat { .. } => true,
        }
    }
}

pub(crate) struct Runtime {
    ports: SessionPorts,
    netlink: netlink::Handle,
    applied: Vec<RoutingMutation>,
}

impl Runtime {
    pub(crate) async fn start(
        config: &SessionConfig,
        ports: SessionPorts,
        netlink: netlink::Handle,
    ) -> io::Result<Self> {
        let mut runtime = Self {
            ports,
            netlink,
            applied: Vec::new(),
        };
        if let Err(e) = runtime.setup(config).await {
            runtime.stop().await;
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
        let desired = self.desired_mutations(next)?;
        self.reconcile(desired).await
    }

    pub(crate) async fn stop(mut self) {
        self.drain_applied().await;
    }

    async fn setup(&mut self, config: &SessionConfig) -> io::Result<()> {
        let desired = self.desired_mutations(config)?;
        self.reconcile(desired).await
    }

    async fn reconcile(&mut self, desired: Vec<RoutingMutation>) -> io::Result<()> {
        let mut index = self.applied.len();
        while index > 0 {
            index -= 1;
            if !desired.contains(&self.applied[index]) {
                let mutation = self.applied[index].clone();
                if mutation.delete(&self.netlink).await {
                    self.applied.remove(index);
                }
            }
        }
        let mut index = 0;
        while index < desired.len() {
            let mutation = &desired[index];
            if self.applied.contains(mutation) {
                index += 1;
            } else if let RoutingMutation::Iptables(rule) = mutation {
                let target = rule.target;
                let table = rule.table;
                let mut rules = Vec::new();
                while let Some(RoutingMutation::Iptables(rule)) = desired.get(index) {
                    if self.applied.contains(&desired[index])
                        || rule.target != target
                        || rule.table != table
                    {
                        break;
                    }
                    rules.push(rule.clone());
                    index += 1;
                }
                apply_iptables_batch(target, table, &rules).await?;
                self.applied
                    .extend(rules.into_iter().map(RoutingMutation::Iptables));
            } else {
                mutation.apply(&self.netlink).await?;
                if !matches!(mutation, RoutingMutation::EnsureIptablesChain { .. }) {
                    self.applied.push(mutation.clone());
                }
                index += 1;
            }
        }
        Ok(())
    }

    async fn drain_applied(&mut self) {
        while let Some(mutation) = self.applied.pop() {
            mutation.delete(&self.netlink).await;
        }
    }

    fn desired_mutations(&self, config: &SessionConfig) -> io::Result<Vec<RoutingMutation>> {
        let mut mutations = Vec::new();
        if config.ip_forward {
            push_unique(
                &mut mutations,
                RoutingMutation::IpForward {
                    downstream: config.downstream.clone(),
                },
            );
        }
        for rule in self.dns_rules(config) {
            push_unique(&mut mutations, RoutingMutation::Iptables(rule));
        }
        push_unique(
            &mut mutations,
            RoutingMutation::IpRule(IpRuleCommand {
                operation: IpOperation::Replace,
                family: IpFamily::Ipv4,
                iif: config.downstream.clone(),
                priority: rule_priority(RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE),
                action: RuleAction::Unreachable,
                table: 0,
                fwmark: None,
            }),
        );
        if config.forward {
            for chain in ["vpnhotspot_acl", "vpnhotspot_stats"] {
                push_unique(
                    &mut mutations,
                    RoutingMutation::EnsureIptablesChain {
                        target: IptablesTarget::Ipv4,
                        table: "filter",
                        chain,
                    },
                );
            }
            for rule in self.forward_rules(config) {
                push_unique(&mut mutations, RoutingMutation::Iptables(rule));
            }
        }
        if config.masquerade == MasqueradeMode::Simple {
            push_unique(
                &mut mutations,
                RoutingMutation::EnsureIptablesChain {
                    target: IptablesTarget::Ipv4,
                    table: "nat",
                    chain: "vpnhotspot_masquerade",
                },
            );
            push_unique(
                &mut mutations,
                RoutingMutation::Iptables(Self::masquerade_chain_rule()),
            );
        }
        let mut upstreams = Vec::new();
        for upstream in &config.upstreams {
            if upstreams.contains(upstream) {
                continue;
            }
            upstreams.push(upstream.clone());
            push_unique(
                &mut mutations,
                RoutingMutation::IpRule(IpRuleCommand {
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
                }),
            );
            match config.masquerade {
                MasqueradeMode::None => {}
                MasqueradeMode::Simple => push_unique(
                    &mut mutations,
                    RoutingMutation::Iptables(Self::upstream_masquerade_rule(config, upstream)),
                ),
                MasqueradeMode::Netd => push_unique(
                    &mut mutations,
                    RoutingMutation::NetdNat {
                        downstream: config.downstream.clone(),
                        upstream: upstream.ifname.clone(),
                    },
                ),
            }
        }
        if config.ipv6_block {
            push_unique(
                &mut mutations,
                RoutingMutation::EnsureIptablesChain {
                    target: IptablesTarget::Ipv6,
                    table: "filter",
                    chain: "vpnhotspot_filter",
                },
            );
            for rule in Self::ipv6_block_rules(config) {
                push_unique(&mut mutations, RoutingMutation::Iptables(rule));
            }
        }
        if let Some(ipv6_nat) = &config.ipv6_nat {
            let ports = self
                .ports
                .ipv6_nat
                .ok_or_else(|| io::Error::other("missing IPv6 NAT ports"))?;
            push_unique(
                &mut mutations,
                RoutingMutation::IpRoute(IpRouteCommand {
                    operation: IpOperation::Replace,
                    route_type: RouteType::Unicast,
                    destination: IpAddr::V6(route_address(ipv6_nat.gateway, ipv6_nat.prefix_len)),
                    prefix_len: ipv6_nat.prefix_len,
                    interface: config.downstream.clone(),
                    table: LOCAL_NETWORK_TABLE,
                }),
            );
            push_unique(
                &mut mutations,
                RoutingMutation::IpAddress(IpAddressCommand {
                    operation: IpOperation::Replace,
                    address: IpAddr::V6(ipv6_nat.gateway),
                    prefix_len: ipv6_nat.prefix_len,
                    interface: config.downstream.clone(),
                }),
            );
            push_unique(
                &mut mutations,
                RoutingMutation::IpRoute(IpRouteCommand {
                    operation: IpOperation::Replace,
                    route_type: RouteType::Local,
                    destination: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                    prefix_len: 0,
                    interface: "lo".to_string(),
                    table: DAEMON_TABLE,
                }),
            );
            push_unique(
                &mut mutations,
                RoutingMutation::IpRule(IpRuleCommand {
                    operation: IpOperation::Replace,
                    family: IpFamily::Ipv6,
                    iif: config.downstream.clone(),
                    priority: rule_priority(RULE_PRIORITY_DAEMON_BASE),
                    action: RuleAction::Lookup,
                    table: DAEMON_TABLE,
                    fwmark: Some((DAEMON_INTERCEPT_FWMARK_VALUE, DAEMON_INTERCEPT_FWMARK_MASK)),
                }),
            );
            for (target, table, chain) in [
                (IptablesTarget::Ipv6, "filter", "vpnhotspot_v6_input"),
                (IptablesTarget::Ipv6, "filter", "vpnhotspot_v6_forward"),
                (IptablesTarget::Ipv6, "filter", "vpnhotspot_v6_output"),
                (IptablesTarget::Ipv6, "mangle", "vpnhotspot_acl"),
                (IptablesTarget::Ipv6, "mangle", "vpnhotspot_v6_tproxy"),
            ] {
                push_unique(
                    &mut mutations,
                    RoutingMutation::EnsureIptablesChain {
                        target,
                        table,
                        chain,
                    },
                );
            }
            for rule in Self::ipv6_nat_filter_rules(config) {
                push_unique(&mut mutations, RoutingMutation::Iptables(rule));
            }
            push_unique(
                &mut mutations,
                RoutingMutation::Iptables(Self::ipv6_nat_acl_drop_rule()),
            );
            for rule in Self::ipv6_nat_tproxy_port_rules(config, ports) {
                push_unique(&mut mutations, RoutingMutation::Iptables(rule));
            }
            push_unique(
                &mut mutations,
                RoutingMutation::Iptables(Self::ipv6_nat_tproxy_acl_rule(config)),
            );
            for rule in Self::ipv6_nat_filter_jump_rules() {
                push_unique(&mut mutations, RoutingMutation::Iptables(rule));
            }
            push_unique(
                &mut mutations,
                RoutingMutation::Iptables(Self::ipv6_nat_prerouting_rule()),
            );
        }
        let mut client_macs_v4 = Vec::new();
        let mut client_ips = Vec::new();
        let mut client_macs_v6 = Vec::new();
        for client in &config.clients {
            if config.forward {
                if !client_macs_v4.contains(&client.mac) {
                    client_macs_v4.push(client.mac);
                    for rule in Self::client_mac_v4_rules(
                        config,
                        &ClientConfig {
                            mac: client.mac,
                            ipv4: Vec::new(),
                        },
                    ) {
                        push_unique(&mut mutations, RoutingMutation::Iptables(rule));
                    }
                }
                for address in &client.ipv4 {
                    if client_ips.contains(address) {
                        continue;
                    }
                    client_ips.push(*address);
                    for rule in Self::client_ip_stats_rules(config, *address) {
                        push_unique(&mut mutations, RoutingMutation::Iptables(rule));
                    }
                }
            }
            if config.ipv6_nat.is_some() && !client_macs_v6.contains(&client.mac) {
                client_macs_v6.push(client.mac);
                push_unique(
                    &mut mutations,
                    RoutingMutation::Iptables(Self::client_mac_v6_rule(&ClientConfig {
                        mac: client.mac,
                        ipv4: Vec::new(),
                    })),
                );
            }
        }
        Ok(mutations)
    }

    fn dns_rules(&self, config: &SessionConfig) -> Vec<IptablesRule> {
        [("tcp", self.ports.dns_tcp), ("udp", self.ports.dns_udp)]
            .into_iter()
            .map(|(protocol, port)| {
                IptablesRule::new(
                    IptablesTarget::Ipv4,
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

    fn forward_rules(&self, config: &SessionConfig) -> Vec<IptablesRule> {
        vec![
            IptablesRule::new(
                IptablesTarget::Ipv4,
                "filter",
                "FORWARD",
                vec!["-j".into(), "vpnhotspot_acl".into()],
            ),
            IptablesRule::new(
                IptablesTarget::Ipv4,
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
                IptablesTarget::Ipv4,
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
                IptablesTarget::Ipv4,
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

    fn masquerade_chain_rule() -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv4,
            "nat",
            "POSTROUTING",
            vec!["-j".into(), "vpnhotspot_masquerade".into()],
        )
    }

    fn upstream_masquerade_rule(config: &SessionConfig, upstream: &UpstreamConfig) -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv4,
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

    fn client_mac_v4_rules(config: &SessionConfig, client: &ClientConfig) -> Vec<IptablesRule> {
        let mac = mac_string(&client.mac);
        vec![
            IptablesRule::new(
                IptablesTarget::Ipv4,
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
                IptablesTarget::Ipv4,
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

    fn client_ip_stats_rules(config: &SessionConfig, address: Ipv4Addr) -> Vec<IptablesRule> {
        let address = address.to_string();
        vec![
            IptablesRule::new(
                IptablesTarget::Ipv4,
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
                IptablesTarget::Ipv4,
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

    fn client_mac_v6_rule(client: &ClientConfig) -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv6,
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

    fn ipv6_block_rules(config: &SessionConfig) -> Vec<IptablesRule> {
        let mut rules = Vec::new();
        for chain in ["INPUT", "FORWARD", "OUTPUT"] {
            rules.push(IptablesRule::new(
                IptablesTarget::Ipv6,
                "filter",
                chain,
                vec!["-j".into(), "vpnhotspot_filter".into()],
            ));
        }
        rules.push(IptablesRule::new(
            IptablesTarget::Ipv6,
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
            IptablesTarget::Ipv6,
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

    fn ipv6_nat_filter_rules(config: &SessionConfig) -> Vec<IptablesRule> {
        vec![
            IptablesRule::new(
                IptablesTarget::Ipv6,
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
                IptablesTarget::Ipv6,
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
                IptablesTarget::Ipv6,
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
                IptablesTarget::Ipv6,
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
                IptablesTarget::Ipv6,
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
                IptablesTarget::Ipv6,
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
                IptablesTarget::Ipv6,
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
            IptablesTarget::Ipv6,
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
                    IptablesTarget::Ipv6,
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
            IptablesTarget::Ipv6,
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
            IptablesRule::new(
                IptablesTarget::Ipv6,
                "filter",
                chain,
                vec!["-j".into(), target.into()],
            )
        })
        .collect()
    }

    fn ipv6_nat_prerouting_rule() -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv6,
            "mangle",
            "PREROUTING",
            vec!["-j".into(), "vpnhotspot_v6_tproxy".into()],
        )
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
        IptablesTarget::Ipv4,
        "mangle",
        "PREROUTING",
        &["-j", "vpnhotspot_dns_tproxy"],
    )
    .await;
    delete_iptables_repeated(
        IptablesTarget::Ipv4,
        "filter",
        "FORWARD",
        &["-j", "vpnhotspot_acl"],
    )
    .await;
    delete_iptables_repeated(
        IptablesTarget::Ipv4,
        "nat",
        "POSTROUTING",
        &["-j", "vpnhotspot_masquerade"],
    )
    .await;
    if let Err(e) = firewall::restore(
        IptablesTarget::Ipv4,
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
        delete_iptables_repeated(
            IptablesTarget::Ipv6,
            "filter",
            chain,
            &["-j", "vpnhotspot_filter"],
        )
        .await;
    }
    for (chain, target) in [
        ("INPUT", "vpnhotspot_v6_input"),
        ("FORWARD", "vpnhotspot_v6_forward"),
        ("OUTPUT", "vpnhotspot_v6_output"),
    ] {
        delete_iptables_repeated(IptablesTarget::Ipv6, "filter", chain, &["-j", target]).await;
    }
    delete_iptables_repeated(
        IptablesTarget::Ipv6,
        "mangle",
        "PREROUTING",
        &["-j", "vpnhotspot_v6_tproxy"],
    )
    .await;
    if let Err(e) = firewall::restore(
        IptablesTarget::Ipv6,
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

fn push_unique(mutations: &mut Vec<RoutingMutation>, mutation: RoutingMutation) {
    if !mutations.contains(&mutation) {
        mutations.push(mutation);
    }
}

async fn apply_iptables_batch(
    target: IptablesTarget,
    table: &str,
    rules: &[IptablesRule],
) -> io::Result<()> {
    let mut lines = Vec::with_capacity(rules.len());
    for rule in rules {
        lines.push(
            rule.insert_line()
                .with_report_context_details("routing.iptables_insert.line", rule.details())?,
        );
    }
    firewall::restore(target, &firewall::restore_input(table, &lines)).await
}

async fn ensure_iptables_chain(target: IptablesTarget, table: &str, chain: &str) {
    match firewall::restore_line("-N", chain, &[]) {
        Ok(line) => {
            let input = firewall::restore_input(table, &[line]);
            if let Err(e) = firewall::restore_status(target, &input).await {
                report::io_with_details(
                    "routing.iptables_new_chain",
                    e,
                    firewall::restore_details(target, &input),
                );
            }
        }
        Err(e) => report::io_with_details(
            "routing.iptables_new_chain",
            e,
            [
                ("binary", target.restore_binary().to_owned()),
                ("table", table.to_owned()),
                ("chain", chain.to_owned()),
            ],
        ),
    }
}

async fn add_ip_forward(downstream: &str) -> io::Result<()> {
    if let Err(e) = run_ndc(
        "ipfwd",
        &["ipfwd", "enable", &format!("vpnhotspot_{downstream}")],
    )
    .await
    {
        eprintln!("ndc ipfwd enable failed: {e}");
        tokio::fs::write("/proc/sys/net/ipv4/ip_forward", b"1").await?;
    }
    Ok(())
}

async fn remove_ip_forward(downstream: &str) -> bool {
    if let Err(e) = run_ndc(
        "ipfwd",
        &["ipfwd", "disable", &format!("vpnhotspot_{downstream}")],
    )
    .await
    {
        report::io_with_details(
            "routing.remove_ip_forward",
            e,
            [("downstream", downstream.to_owned())],
        );
        false
    } else {
        true
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
            if !is_missing_address(&e) {
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

async fn delete_rule_result(handle: &netlink::Handle, mut command: IpRuleCommand) -> bool {
    command.operation = IpOperation::Delete;
    match apply_rule_command(handle, &command).await {
        Ok(()) => true,
        Err(e) if is_missing(&e) => true,
        Err(e) => {
            report::io_with_details("routing.delete_rule", e, rule_details(&command));
            false
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

async fn delete_route_result(handle: &netlink::Handle, mut command: IpRouteCommand) -> bool {
    command.operation = IpOperation::Delete;
    match apply_route_command(handle, &command).await {
        Ok(()) => true,
        Err(e) if is_missing(&e) => true,
        Err(e) => {
            report::io_with_details("routing.delete_route", e, route_details(&command));
            false
        }
    }
}

async fn apply_address(handle: &netlink::Handle, command: IpAddressCommand) -> io::Result<()> {
    match apply_address_command(handle, &command).await {
        Err(e) if error_errno(&e) == Some(libc::EEXIST) => Ok(()),
        result => result,
    }
}

async fn delete_address_result(handle: &netlink::Handle, mut command: IpAddressCommand) -> bool {
    command.operation = IpOperation::Delete;
    match apply_address_command(handle, &command).await {
        Ok(()) => true,
        Err(e) if is_missing_address(&e) => true,
        Err(e) => {
            report::io_with_details("routing.delete_address", e, address_details(&command));
            false
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

fn is_missing_address(error: &io::Error) -> bool {
    is_missing(error) || error_errno(error) == Some(libc::EADDRNOTAVAIL)
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

async fn delete_iptables_repeated(target: IptablesTarget, table: &str, chain: &str, args: &[&str]) {
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    let input = match firewall::restore_line("-D", chain, &args) {
        Ok(line) => firewall::restore_input(table, &[line]),
        Err(e) => {
            report::io_with_details(
                "routing.delete_iptables_repeated",
                e,
                [
                    ("binary", target.restore_binary().to_owned()),
                    ("table", table.to_owned()),
                    ("chain", chain.to_owned()),
                    ("args", args.join(" ")),
                ],
            );
            return;
        }
    };
    loop {
        match firewall::restore_status(target, &input).await {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                report::io_with_details(
                    "routing.delete_iptables_repeated",
                    e,
                    firewall::restore_details(target, &input),
                );
                break;
            }
        }
    }
}

async fn run_ndc(name: &str, args: &[&str]) -> io::Result<()> {
    let details = vec![
        ("binary".to_owned(), firewall::NDC.to_owned()),
        ("args".to_owned(), args.join(" ")),
    ];
    let output = Command::new(firewall::NDC)
        .args(args)
        .output()
        .await
        .with_report_context_details("routing.ndc.spawn", details.clone())?;
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
        .with_report_context_details("routing.ndc.status", details)
    }
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
