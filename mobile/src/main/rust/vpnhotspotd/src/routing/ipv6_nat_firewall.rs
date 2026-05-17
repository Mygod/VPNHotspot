use crate::firewall::IptablesTarget;
use vpnhotspotd::shared::icmp_nat::icmp_echo_rule_args;
use vpnhotspotd::shared::model::{
    Ipv6NatConfig, Ipv6NatPorts, SessionConfig, DAEMON_ICMP_NFQUEUE_NUM, DAEMON_REPLY_MARK,
    DAEMON_REPLY_MARK_MASK, DAEMON_UDP_TPROXY_ADDRESS,
};

use super::iptables::{IptablesChain, IptablesRule};
use super::{push_unique, RoutingMutation};

pub(super) struct Ipv6NatFirewall;

impl Ipv6NatFirewall {
    const BLOCK_FILTER_CHAIN: IptablesChain =
        IptablesChain::new(IptablesTarget::Ipv6, "filter", "vpnhotspot_filter");
    const NAT_FILTER_JUMPS: [(&'static str, IptablesChain); 3] = [
        (
            "INPUT",
            IptablesChain::new(IptablesTarget::Ipv6, "filter", "vpnhotspot_v6_input"),
        ),
        (
            "FORWARD",
            IptablesChain::new(IptablesTarget::Ipv6, "filter", "vpnhotspot_v6_forward"),
        ),
        (
            "OUTPUT",
            IptablesChain::new(IptablesTarget::Ipv6, "filter", "vpnhotspot_v6_output"),
        ),
    ];
    pub(super) const NAT_MANGLE_CHAINS: [IptablesChain; 4] = [
        IptablesChain::new(IptablesTarget::Ipv6, "mangle", "vpnhotspot_acl"),
        IptablesChain::new(IptablesTarget::Ipv6, "mangle", "vpnhotspot_v6_acl_gate"),
        IptablesChain::new(IptablesTarget::Ipv6, "mangle", "vpnhotspot_v6_protocols"),
        IptablesChain::new(IptablesTarget::Ipv6, "mangle", "vpnhotspot_v6_tproxy"),
    ];

    pub(super) fn append_session_mutations(
        mutations: &mut Vec<RoutingMutation>,
        config: &SessionConfig,
        ipv6_nat: &Ipv6NatConfig,
        ports: Ipv6NatPorts,
    ) {
        for chain in Self::NAT_FILTER_JUMPS
            .into_iter()
            .map(|(_, chain)| chain)
            .chain(Self::NAT_MANGLE_CHAINS)
        {
            push_unique(
                mutations,
                RoutingMutation::EnsureIptablesChain {
                    target: chain.target,
                    table: chain.table,
                    chain: chain.chain,
                },
            );
        }
        for rule in Self::filter_rules(config) {
            push_unique(mutations, RoutingMutation::Iptables(rule));
        }
        push_unique(
            mutations,
            RoutingMutation::Iptables(Self::acl_gate_rule(config)),
        );
        if ports.icmp_echo {
            push_unique(
                mutations,
                RoutingMutation::Iptables(Self::icmp_echo_rule(config, ipv6_nat)),
            );
        }
        for rule in Self::tproxy_port_rules(config, ports) {
            push_unique(mutations, RoutingMutation::Iptables(rule));
        }
        // Iptables rules are installed with -I; these land before the ACL gate.
        for rule in Self::tproxy_icmpv6_control_rules(config) {
            push_unique(mutations, RoutingMutation::Iptables(rule));
        }
        for rule in Self::filter_jump_rules() {
            push_unique(mutations, RoutingMutation::Iptables(rule));
        }
        push_unique(
            mutations,
            RoutingMutation::Iptables(Self::prerouting_rule()),
        );
    }

    fn filter_rules(config: &SessionConfig) -> Vec<IptablesRule> {
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

    pub(super) fn base_rules() -> [IptablesRule; 3] {
        [
            Self::acl_drop_rule(),
            Self::tproxy_protocols_rule(),
            Self::tproxy_acl_rule(),
        ]
    }

    fn acl_drop_rule() -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv6,
            "mangle",
            "vpnhotspot_acl",
            vec!["-j".into(), "DROP".into()],
        )
    }

    fn tproxy_port_rules(config: &SessionConfig, ports: Ipv6NatPorts) -> Vec<IptablesRule> {
        [("tcp", ports.tcp), ("udp", ports.udp)]
            .into_iter()
            .map(|(protocol, port)| {
                let mut args = vec![
                    "-i".into(),
                    config.downstream.clone(),
                    "-p".into(),
                    protocol.into(),
                    "-j".into(),
                    "TPROXY".into(),
                ];
                if protocol == "udp" {
                    // Keep listener socket lookup disjoint from exact-bound UDP reply sockets.
                    args.extend(["--on-ip".into(), DAEMON_UDP_TPROXY_ADDRESS.to_string()]);
                }
                args.extend([
                    "--on-port".into(),
                    port.to_string(),
                    "--tproxy-mark".into(),
                    "0x10000000/0x10000000".into(),
                ]);
                IptablesRule::new(
                    IptablesTarget::Ipv6,
                    "mangle",
                    "vpnhotspot_v6_protocols",
                    args,
                )
            })
            .collect()
    }

    fn icmp_echo_rule(config: &SessionConfig, ipv6_nat: &Ipv6NatConfig) -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv6,
            "mangle",
            "vpnhotspot_v6_protocols",
            icmp_echo_rule_args(
                config.downstream.clone(),
                ipv6_nat.gateway.address(),
                DAEMON_ICMP_NFQUEUE_NUM,
            ),
        )
    }

    fn acl_gate_rule(config: &SessionConfig) -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv6,
            "mangle",
            "vpnhotspot_v6_acl_gate",
            vec![
                "-i".into(),
                config.downstream.clone(),
                "-j".into(),
                "vpnhotspot_acl".into(),
            ],
        )
    }

    fn tproxy_acl_rule() -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv6,
            "mangle",
            "vpnhotspot_v6_tproxy",
            vec!["-j".into(), "vpnhotspot_v6_acl_gate".into()],
        )
    }

    fn tproxy_protocols_rule() -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv6,
            "mangle",
            "vpnhotspot_v6_tproxy",
            vec!["-j".into(), "vpnhotspot_v6_protocols".into()],
        )
    }

    fn tproxy_icmpv6_control_rules(config: &SessionConfig) -> Vec<IptablesRule> {
        ["133", "135", "136"]
            .into_iter()
            .map(|icmpv6_type| {
                IptablesRule::new(
                    IptablesTarget::Ipv6,
                    "mangle",
                    "vpnhotspot_v6_tproxy",
                    vec![
                        "-i".into(),
                        config.downstream.clone(),
                        "-p".into(),
                        "icmpv6".into(),
                        "--icmpv6-type".into(),
                        icmpv6_type.into(),
                        "-j".into(),
                        "RETURN".into(),
                    ],
                )
            })
            .collect()
    }

    pub(super) fn filter_jump_rules() -> Vec<IptablesRule> {
        Self::NAT_FILTER_JUMPS
            .into_iter()
            .map(|(base_chain, target_chain)| {
                IptablesRule::new(
                    target_chain.target,
                    target_chain.table,
                    base_chain,
                    vec!["-j".into(), target_chain.chain.into()],
                )
            })
            .collect()
    }

    pub(super) fn prerouting_rule() -> IptablesRule {
        IptablesRule::new(
            IptablesTarget::Ipv6,
            "mangle",
            "PREROUTING",
            vec!["-j".into(), "vpnhotspot_v6_tproxy".into()],
        )
    }

    pub(super) fn clean_filter_input() -> String {
        Self::clean_chains_input(
            "filter",
            [Self::BLOCK_FILTER_CHAIN]
                .into_iter()
                .chain(Self::NAT_FILTER_JUMPS.into_iter().map(|(_, chain)| chain)),
        )
    }

    pub(super) fn clean_mangle_input() -> String {
        Self::clean_chains_input("mangle", Self::NAT_MANGLE_CHAINS)
    }

    fn clean_chains_input(table: &str, chains: impl IntoIterator<Item = IptablesChain>) -> String {
        let chains = chains.into_iter().collect::<Vec<_>>();
        let mut input = String::new();
        input.push('*');
        input.push_str(table);
        input.push('\n');
        for chain in &chains {
            input.push(':');
            input.push_str(chain.chain);
            input.push_str(" - [0:0]\n");
        }
        for chain in chains {
            input.push_str("-X ");
            input.push_str(chain.chain);
            input.push('\n');
        }
        input.push_str("COMMIT\n");
        input
    }
}
