use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use cidr::Ipv4Inet;
use rtnetlink::packet_route::IpProtocol;

use crate::{firewall::IptablesTarget, netlink, report};
use vpnhotspotd::shared::downstream::DownstreamIpv4;
use vpnhotspotd::shared::model::{
    mac_string, ClientDnsPorts, ClientIpv6NatPorts, Ipv6NatConfig, Ipv6NatPorts, SessionConfig,
    SessionPorts, UpstreamConfig, UpstreamRole, DAEMON_INTERCEPT_FWMARK_MASK,
    DAEMON_INTERCEPT_FWMARK_VALUE, DAEMON_TABLE, LOCAL_NETWORK_TABLE,
};
use vpnhotspotd::shared::proto::daemon::MasqueradeMode;

use super::iptables::IptablesRule;
use super::ipv6_nat_firewall::Ipv6NatFirewall;
use super::ipv6_nat_intercept::Ipv6NatInterceptMode;
use super::netlink_commands::{
    IpAddressCommand, IpCommand, IpFamily, IpOperation, IpRouteCommand, IpRuleCommand, RouteType,
    RuleAction,
};
use super::{
    push_unique, rule_priority, RoutingMutation, Runtime, RULE_PRIORITY_DAEMON_BASE,
    RULE_PRIORITY_UPSTREAM_BASE, RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE,
    RULE_PRIORITY_UPSTREAM_FALLBACK_BASE,
};

impl Runtime {
    pub(super) async fn desired_mutations(&self, config: &SessionConfig) -> Vec<RoutingMutation> {
        let mut mutations = Vec::new();
        if config.ip_forward {
            push_unique(
                &mut mutations,
                RoutingMutation::IpForward {
                    downstream: config.downstream.clone(),
                },
            );
        }
        push_unique(
            &mut mutations,
            RoutingMutation::EnsureIptablesChain {
                target: IptablesTarget::Ipv4,
                table: "filter",
                chain: "vpnhotspot_dns_input",
            },
        );
        for rule in self.dns_rules(config) {
            push_unique(&mut mutations, RoutingMutation::Iptables(rule));
        }
        push_unique(
            &mut mutations,
            RoutingMutation::Ip(IpCommand::Rule(IpRuleCommand {
                operation: IpOperation::Replace,
                family: IpFamily::Ipv4,
                iif: config.downstream.clone(),
                priority: rule_priority(RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE),
                action: RuleAction::Unreachable,
                table: 0,
                fwmark: None,
                ip_protocol: None,
            })),
        );
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
        // The client-specific forwarding filters and counters are added from neighbour snapshots.
        for rule in self.forward_rules(config) {
            push_unique(&mut mutations, RoutingMutation::Iptables(rule));
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
            // Upstream-specific MASQUERADE rules are added from upstream snapshots.
        }
        let mut seen_upstream_interfaces = HashSet::new();
        for (interfaces, role) in [
            (&config.primary_upstream_interfaces, UpstreamRole::Primary),
            (&config.fallback_upstream_interfaces, UpstreamRole::Fallback),
        ] {
            for ifname in interfaces {
                if !seen_upstream_interfaces.insert(ifname.as_str()) {
                    continue;
                }
                let ifindex = match netlink::link_index(&self.netlink, ifname).await {
                    Ok(ifindex) => ifindex,
                    Err(e) if netlink::is_missing_link(&e) => continue,
                    Err(e) => {
                        report::io_with_details(
                            "routing.resolve_upstream_index",
                            e,
                            [("upstream", ifname.as_str())],
                        );
                        continue;
                    }
                };
                let upstream = UpstreamConfig {
                    ifname: ifname.clone(),
                    role,
                };
                push_unique(
                    &mut mutations,
                    RoutingMutation::Ip(IpCommand::Rule(IpRuleCommand {
                        operation: IpOperation::Replace,
                        family: IpFamily::Ipv4,
                        iif: config.downstream.clone(),
                        priority: rule_priority(match upstream.role {
                            UpstreamRole::Primary => RULE_PRIORITY_UPSTREAM_BASE,
                            UpstreamRole::Fallback => RULE_PRIORITY_UPSTREAM_FALLBACK_BASE,
                        }),
                        action: RuleAction::Lookup,
                        // https://android.googlesource.com/platform/system/netd/+/android-5.0.0_r1/server/RouteController.h#37
                        table: 1000 + ifindex,
                        fwmark: None,
                        ip_protocol: None,
                    })),
                );
                match config.masquerade {
                    MasqueradeMode::None => {}
                    MasqueradeMode::Simple => push_unique(
                        &mut mutations,
                        RoutingMutation::Iptables(self.upstream_masquerade_rule(&upstream)),
                    ),
                    MasqueradeMode::Netd => push_unique(
                        &mut mutations,
                        RoutingMutation::NetdNat {
                            downstream: config.downstream.clone(),
                            upstream: upstream.ifname,
                        },
                    ),
                }
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
        let ipv6_nat = config.ipv6_nat.as_ref().zip(self.ports.ipv6_nat.as_ref());
        if let Some((ipv6_nat, ports)) = ipv6_nat {
            push_unique(
                &mut mutations,
                RoutingMutation::Ip(IpCommand::Route(IpRouteCommand {
                    operation: IpOperation::Replace,
                    route_type: RouteType::Unicast,
                    destination: IpAddr::V6(ipv6_nat.gateway.first_address()),
                    prefix_len: ipv6_nat.gateway.network_length(),
                    interface: config.downstream.clone(),
                    table: LOCAL_NETWORK_TABLE,
                })),
            );
            push_unique(
                &mut mutations,
                RoutingMutation::Ip(IpCommand::Address(IpAddressCommand {
                    operation: IpOperation::Replace,
                    address: IpAddr::V6(ipv6_nat.gateway.address()),
                    prefix_len: ipv6_nat.gateway.network_length(),
                    interface: config.downstream.clone(),
                })),
            );
            push_unique(
                &mut mutations,
                RoutingMutation::Ip(IpCommand::Route(IpRouteCommand {
                    operation: IpOperation::Replace,
                    route_type: RouteType::Local,
                    destination: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                    prefix_len: 0,
                    interface: "lo".to_string(),
                    table: DAEMON_TABLE,
                })),
            );
            match self.ipv6_nat_intercept_mode {
                Ipv6NatInterceptMode::ProtocolRules => {
                    for (enabled, protocol) in [
                        (
                            ports.clients.iter().any(|client| client.tcp.is_some()),
                            IpProtocol::Tcp,
                        ),
                        (
                            ports.clients.iter().any(|client| client.udp.is_some()),
                            IpProtocol::Udp,
                        ),
                    ] {
                        if enabled {
                            push_unique(
                                &mut mutations,
                                RoutingMutation::Ip(IpCommand::Rule(IpRuleCommand {
                                    operation: IpOperation::Replace,
                                    family: IpFamily::Ipv6,
                                    iif: config.downstream.clone(),
                                    priority: rule_priority(RULE_PRIORITY_DAEMON_BASE),
                                    action: RuleAction::Lookup,
                                    table: DAEMON_TABLE,
                                    fwmark: None,
                                    ip_protocol: Some(protocol),
                                })),
                            );
                        }
                    }
                }
                Ipv6NatInterceptMode::FwmarkFallback => push_unique(
                    &mut mutations,
                    RoutingMutation::Ip(IpCommand::Rule(IpRuleCommand {
                        operation: IpOperation::Replace,
                        family: IpFamily::Ipv6,
                        iif: config.downstream.clone(),
                        priority: rule_priority(RULE_PRIORITY_DAEMON_BASE),
                        action: RuleAction::Lookup,
                        table: DAEMON_TABLE,
                        fwmark: Some((DAEMON_INTERCEPT_FWMARK_VALUE, DAEMON_INTERCEPT_FWMARK_MASK)),
                        ip_protocol: None,
                    })),
                ),
            }
            Ipv6NatFirewall::append_session_mutations(
                &mut mutations,
                config,
                ipv6_nat,
                ports,
                self.ipv6_nat_intercept_mode,
            );
        }
        let mut client_macs_v4 = Vec::new();
        let mut client_ips = Vec::new();
        let mut client_macs_v6 = Vec::new();
        for client in &config.clients {
            if !client_macs_v4.contains(&client.mac) {
                client_macs_v4.push(client.mac);
                for rule in Self::client_mac_v4_rules(config, client.mac) {
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
            if ipv6_nat.is_some() && !client_macs_v6.contains(&client.mac) {
                client_macs_v6.push(client.mac);
                push_unique(
                    &mut mutations,
                    RoutingMutation::Iptables(Self::client_mac_v6_rule(client.mac)),
                );
            }
        }
        mutations
    }

    pub(super) fn committed_ports(&self, config: &SessionConfig) -> SessionPorts {
        let mut dns = Vec::new();
        for ports in &self.ports.dns {
            let tcp = ports
                .tcp
                .filter(|port| self.dns_port_committed(config, ports.mac, "tcp", *port));
            let udp = ports
                .udp
                .filter(|port| self.dns_port_committed(config, ports.mac, "udp", *port));
            if tcp.is_some() || udp.is_some() {
                dns.push(ClientDnsPorts {
                    mac: ports.mac,
                    tcp,
                    udp,
                });
            }
        }
        let ipv6_nat = config.ipv6_nat.as_ref().and_then(|ipv6_nat| {
            let ports = self.ports.ipv6_nat.as_ref()?;
            let mut clients = Vec::new();
            let session_rules_committed = self.has_applied_iptables_rules(
                Ipv6NatFirewall::local_special_return_rules(config, ipv6_nat),
            ) && self
                .has_applied_iptables_rule(Ipv6NatFirewall::filter_input_jump_rule())
                && self.has_applied_iptables_rules(Ipv6NatFirewall::input_filter_rules(config));
            for ports in &ports.clients {
                let tcp = ports.tcp.filter(|port| {
                    self.ipv6_nat_port_committed(
                        config,
                        ipv6_nat,
                        ports.mac,
                        "tcp",
                        *port,
                        session_rules_committed,
                    )
                });
                let udp = ports.udp.filter(|port| {
                    self.ipv6_nat_port_committed(
                        config,
                        ipv6_nat,
                        ports.mac,
                        "udp",
                        *port,
                        session_rules_committed,
                    )
                });
                if tcp.is_some() || udp.is_some() {
                    clients.push(ClientIpv6NatPorts {
                        mac: ports.mac,
                        tcp,
                        udp,
                    });
                }
            }
            if clients.is_empty() {
                None
            } else {
                Some(Ipv6NatPorts {
                    clients,
                    icmp_echo: ports.icmp_echo
                        && self.applied.contains(&RoutingMutation::Iptables(
                            Ipv6NatFirewall::icmp_echo_rule(config, ipv6_nat),
                        )),
                })
            }
        });
        SessionPorts { dns, ipv6_nat }
    }

    fn dns_port_committed(
        &self,
        config: &SessionConfig,
        mac: [u8; 6],
        protocol: &str,
        port: u16,
    ) -> bool {
        self.has_applied_iptables_rule(Self::dns_input_jump_rule())
            && self.has_applied_iptables_rules(
                std::iter::once(self.dns_port_rule(config, mac, protocol, port))
                    .chain(self.dns_port_guard_rules(config, protocol, port)),
            )
    }

    fn ipv6_nat_port_committed(
        &self,
        config: &SessionConfig,
        ipv6_nat: &Ipv6NatConfig,
        mac: [u8; 6],
        protocol: &str,
        port: u16,
        session_rules_committed: bool,
    ) -> bool {
        session_rules_committed
            && self.has_applied_iptables_rules(Ipv6NatFirewall::gateway_dns_prelude_rules(
                config, ipv6_nat, protocol,
            ))
            && self.has_applied_iptables_rule(Ipv6NatFirewall::tproxy_listener_rule(
                config,
                mac,
                protocol,
                port,
                self.ipv6_nat_intercept_mode,
            ))
    }

    fn has_applied_iptables_rule(&self, rule: IptablesRule) -> bool {
        self.applied.contains(&RoutingMutation::Iptables(rule))
    }

    fn has_applied_iptables_rules(&self, rules: impl IntoIterator<Item = IptablesRule>) -> bool {
        rules
            .into_iter()
            .all(|rule| self.has_applied_iptables_rule(rule))
    }

    fn dns_rules(&self, config: &SessionConfig) -> Vec<IptablesRule> {
        let mut rules = Vec::new();
        let mut client_macs = Vec::new();
        for client in &config.clients {
            if client_macs.contains(&client.mac) {
                continue;
            }
            client_macs.push(client.mac);
            let Some(ports) = self.ports.dns.iter().find(|ports| ports.mac == client.mac) else {
                continue;
            };
            for (protocol, port) in [("tcp", ports.tcp), ("udp", ports.udp)] {
                let Some(port) = port else {
                    continue;
                };
                rules.push(self.dns_port_rule(config, client.mac, protocol, port));
                let [allow, reject] = self.dns_port_guard_rules(config, protocol, port);
                rules.push(reject);
                rules.push(allow);
            }
        }
        rules.push(Self::dns_input_jump_rule());
        rules.extend(["tcp", "udp"].into_iter().map(|protocol| {
            IptablesRule::new(
                IptablesTarget::Ipv4,
                "filter",
                "vpnhotspot_dns_input",
                vec![
                    "-i".into(),
                    config.downstream.clone(),
                    "-p".into(),
                    protocol.into(),
                    "-d".into(),
                    self.downstream_ipv4.address.to_string(),
                    "--dport".into(),
                    "53".into(),
                    "-j".into(),
                    "REJECT".into(),
                    "--reject-with".into(),
                    if protocol == "tcp" {
                        "tcp-reset".into()
                    } else {
                        "icmp-port-unreachable".into()
                    },
                ],
            )
        }));
        rules
    }

    fn dns_port_rule(
        &self,
        config: &SessionConfig,
        mac: [u8; 6],
        protocol: &str,
        port: u16,
    ) -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv4,
            "nat",
            "PREROUTING",
            vec![
                "-i".into(),
                config.downstream.clone(),
                "-p".into(),
                protocol.into(),
                "-m".into(),
                "mac".into(),
                "--mac-source".into(),
                mac_string(&mac),
                "-d".into(),
                self.downstream_ipv4.address.to_string(),
                "--dport".into(),
                "53".into(),
                "-j".into(),
                "DNAT".into(),
                "--to-destination".into(),
                format!(":{port}"),
            ],
        )
    }

    fn dns_port_guard_rules(
        &self,
        config: &SessionConfig,
        protocol: &str,
        port: u16,
    ) -> [IptablesRule; 2] {
        let base_args = [
            "-i".to_owned(),
            config.downstream.clone(),
            "-p".to_owned(),
            protocol.to_owned(),
            "-d".to_owned(),
            self.downstream_ipv4.address.to_string(),
            "--dport".to_owned(),
            port.to_string(),
        ];
        let mut allow_args = base_args.to_vec();
        allow_args.extend([
            "-m".to_owned(),
            "conntrack".to_owned(),
            "--ctorigdst".to_owned(),
            self.downstream_ipv4.address.to_string(),
            "--ctorigdstport".to_owned(),
            "53".to_owned(),
            "-j".to_owned(),
            "RETURN".to_owned(),
        ]);
        let mut reject_args = base_args.to_vec();
        reject_args.extend([
            "-j".to_owned(),
            "REJECT".to_owned(),
            "--reject-with".to_owned(),
            if protocol == "tcp" {
                "tcp-reset".to_owned()
            } else {
                "icmp-port-unreachable".to_owned()
            },
        ]);
        [
            IptablesRule::new(
                IptablesTarget::Ipv4,
                "filter",
                "vpnhotspot_dns_input",
                allow_args,
            ),
            IptablesRule::new(
                IptablesTarget::Ipv4,
                "filter",
                "vpnhotspot_dns_input",
                reject_args,
            ),
        ]
    }

    fn dns_input_jump_rule() -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv4,
            "filter",
            "INPUT",
            vec!["-j".into(), "vpnhotspot_dns_input".into()],
        )
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
                // Ensure blocking works before client-specific allow rules are added.
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

    fn upstream_masquerade_rule(&self, upstream: &UpstreamConfig) -> IptablesRule {
        // Specifying -i would not work in POSTROUTING.
        IptablesRule::new(
            IptablesTarget::Ipv4,
            "nat",
            "vpnhotspot_masquerade",
            vec![
                "-s".into(),
                host_subnet(self.downstream_ipv4),
                "-o".into(),
                upstream.ifname.clone(),
                "-j".into(),
                "MASQUERADE".into(),
            ],
        )
    }

    fn client_mac_v4_rules(config: &SessionConfig, mac: [u8; 6]) -> Vec<IptablesRule> {
        let mac = mac_string(&mac);
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

    pub(super) fn client_ip_stats_rules(
        config: &SessionConfig,
        address: Ipv4Addr,
    ) -> Vec<IptablesRule> {
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

    fn client_mac_v6_rule(mac: [u8; 6]) -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv6,
            "mangle",
            "vpnhotspot_acl",
            vec![
                "-m".into(),
                "mac".into(),
                "--mac-source".into(),
                mac_string(&mac),
                "-j".into(),
                "RETURN".into(),
            ],
        )
    }

    fn ipv6_block_rules(config: &SessionConfig) -> Vec<IptablesRule> {
        let mut rules = Self::ipv6_block_jump_rules();
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

    pub(super) fn ipv6_block_jump_rules() -> Vec<IptablesRule> {
        ["INPUT", "FORWARD", "OUTPUT"]
            .into_iter()
            .map(|chain| {
                IptablesRule::new(
                    IptablesTarget::Ipv6,
                    "filter",
                    chain,
                    vec!["-j".into(), "vpnhotspot_filter".into()],
                )
            })
            .collect()
    }
}

fn host_subnet(downstream_ipv4: DownstreamIpv4) -> String {
    let subnet = Ipv4Inet::new(downstream_ipv4.address, downstream_ipv4.prefix_len)
        .expect("downstream IPv4 prefix length must be <= 32");
    format!("{}/{}", subnet.first_address(), subnet.network_length())
}
