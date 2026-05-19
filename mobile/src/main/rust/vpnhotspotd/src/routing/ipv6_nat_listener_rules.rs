use crate::firewall::IptablesTarget;
use vpnhotspotd::shared::model::{
    mac_string, Ipv6NatConfig, Ipv6NatPorts, SessionConfig, DAEMON_INTERCEPT_FWMARK_MASK,
    DAEMON_INTERCEPT_FWMARK_VALUE, DAEMON_TPROXY_ADDRESS,
};

use super::iptables::IptablesRule;
use super::ipv6_nat_firewall::Ipv6NatFirewall;
use super::ipv6_nat_intercept::Ipv6NatInterceptMode;

impl Ipv6NatFirewall {
    pub(super) fn tproxy_port_rules(
        config: &SessionConfig,
        ports: &Ipv6NatPorts,
        intercept_mode: Ipv6NatInterceptMode,
    ) -> Vec<IptablesRule> {
        let mut rules = Vec::new();
        let mut client_macs = Vec::new();
        for client in &config.clients {
            if client_macs.contains(&client.mac) {
                continue;
            }
            client_macs.push(client.mac);
            let Some(ports) = ports.clients.iter().find(|ports| ports.mac == client.mac) else {
                continue;
            };
            for (protocol, port) in [("tcp", ports.tcp), ("udp", ports.udp)] {
                let Some(port) = port else {
                    continue;
                };
                rules.push(Self::tproxy_listener_rule(
                    config,
                    client.mac,
                    protocol,
                    port,
                    intercept_mode,
                ));
            }
        }
        rules
    }

    pub(super) fn tproxy_listener_rule(
        config: &SessionConfig,
        mac: [u8; 6],
        protocol: &str,
        port: u16,
        intercept_mode: Ipv6NatInterceptMode,
    ) -> IptablesRule {
        let mut args = vec![
            "-i".into(),
            config.downstream.clone(),
            "-p".into(),
            protocol.into(),
            "-m".into(),
            "mac".into(),
            "--mac-source".into(),
            mac_string(&mac),
        ];
        args.extend([
            "-j".into(),
            "TPROXY".into(),
            "--on-ip".into(),
            DAEMON_TPROXY_ADDRESS.to_string(),
            "--on-port".into(),
            port.to_string(),
        ]);
        if intercept_mode == Ipv6NatInterceptMode::FwmarkFallback {
            args.extend([
                "--tproxy-mark".into(),
                format!(
                    "0x{DAEMON_INTERCEPT_FWMARK_VALUE:08x}/0x{DAEMON_INTERCEPT_FWMARK_MASK:08x}"
                ),
            ]);
        }
        IptablesRule::new(
            IptablesTarget::Ipv6,
            "mangle",
            "vpnhotspot_v6_protocols",
            args,
        )
    }

    pub(super) fn local_special_return_rules(
        config: &SessionConfig,
        ipv6_nat: &Ipv6NatConfig,
    ) -> Vec<IptablesRule> {
        Self::local_special_destinations(ipv6_nat)
            .into_iter()
            .map(|destination| {
                IptablesRule::new(
                    IptablesTarget::Ipv6,
                    "mangle",
                    "vpnhotspot_v6_tproxy",
                    vec![
                        "-i".into(),
                        config.downstream.clone(),
                        "-d".into(),
                        destination,
                        "-j".into(),
                        "RETURN".into(),
                    ],
                )
            })
            .collect()
    }

    pub(super) fn gateway_dns_prelude_rules(
        config: &SessionConfig,
        ipv6_nat: &Ipv6NatConfig,
        protocol: &str,
    ) -> [IptablesRule; 2] {
        let base_args = [
            "-i".to_owned(),
            config.downstream.clone(),
            "-p".to_owned(),
            protocol.to_owned(),
            "-d".to_owned(),
            ipv6_nat.gateway.address().to_string(),
            "--dport".to_owned(),
            "53".to_owned(),
            "-j".to_owned(),
        ];
        let mut protocol_args = base_args.to_vec();
        protocol_args.push("vpnhotspot_v6_protocols".to_owned());
        let mut acl_args = base_args.to_vec();
        acl_args.push("vpnhotspot_v6_acl_gate".to_owned());
        [
            IptablesRule::new(
                IptablesTarget::Ipv6,
                "mangle",
                "vpnhotspot_v6_tproxy",
                protocol_args,
            ),
            IptablesRule::new(
                IptablesTarget::Ipv6,
                "mangle",
                "vpnhotspot_v6_tproxy",
                acl_args,
            ),
        ]
    }

    pub(super) fn enabled_listener_protocols(
        ports: &Ipv6NatPorts,
    ) -> impl Iterator<Item = &'static str> {
        [
            (
                ports.clients.iter().any(|client| client.tcp.is_some()),
                "tcp",
            ),
            (
                ports.clients.iter().any(|client| client.udp.is_some()),
                "udp",
            ),
        ]
        .into_iter()
        .filter_map(|(enabled, protocol)| enabled.then_some(protocol))
    }

    fn local_special_destinations(ipv6_nat: &Ipv6NatConfig) -> Vec<String> {
        vec![
            format!(
                "{}/{}",
                ipv6_nat.gateway.first_address(),
                ipv6_nat.gateway.network_length()
            ),
            "fe80::/10".to_owned(),
            "ff00::/8".to_owned(),
            "::/127".to_owned(),
        ]
    }
}
