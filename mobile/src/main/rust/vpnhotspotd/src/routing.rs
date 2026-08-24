use std::io;

use crate::{firewall::IptablesTarget, netlink, platform, report};
use vpnhotspotd::shared::downstream::DownstreamIpv4;
use vpnhotspotd::shared::ipv4_forward_counter::changed_ipv4_forward_counter_addresses;
use vpnhotspotd::shared::model::{SessionConfig, SessionPorts};
use vpnhotspotd::shared::proto::daemon::CleanRoutingCommand;

mod desired;
mod firewall_cleanup;
mod iptables;
mod ipv6_nat_firewall;
mod ipv6_nat_intercept;
mod ipv6_nat_listener_rules;
mod ndc;
mod netlink_commands;
mod static_addresses;

use iptables::{
    apply_iptables_batch, ensure_iptables_chain, ensure_iptables_chain_result, IptablesRule,
};
use ipv6_nat_firewall::Ipv6NatFirewall;
use ipv6_nat_intercept::Ipv6NatInterceptMode;
use ndc::{add_ip_forward, remove_ip_forward, run_ndc};
use netlink_commands::{delete_rule_repeated, IpCommand, IpFamily};
use static_addresses::clean_ip;

pub(crate) use static_addresses::replace_static_addresses;

// AOSP local-network/tethering priorities are 20000/21000 since Android 12 and
// 17000/18000 on Android 10/11. Keep VPNHotspot rules inside that gap.
// This also works for Wi-Fi direct where there's no system tethering rule to override.
//
// Sources:
// https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/server/RouteController.cpp#65
// https://android.googlesource.com/platform/system/netd/+/android-17.0.0_r1/server/RouteController.h#49
const RULE_PRIORITY_DAEMON_BASE: u32 = 20600;
const RULE_PRIORITY_UPSTREAM_BASE: u32 = 20700;
const RULE_PRIORITY_UPSTREAM_FALLBACK_BASE: u32 = 20800;
const RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE: u32 = 20900;

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
    Ip(IpCommand),
    NetdNat {
        downstream: String,
        upstream: String,
    },
}

impl RoutingMutation {
    async fn apply(&self, connection: &mut netlink::RequestConnection) -> io::Result<()> {
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
            Self::Ip(command) => command.apply(connection).await,
            Self::NetdNat {
                downstream,
                upstream,
            } => {
                // 0 means that there are no interface addresses coming after, which is unused anyway.
                run_ndc("Nat", &["nat", "enable", downstream, upstream, "0"]).await
            }
        }
    }

    async fn delete(&self, connection: &mut netlink::RequestConnection) -> bool {
        match self {
            Self::EnsureIptablesChain { .. } => true,
            Self::Iptables(rule) => rule.delete().await,
            Self::IpForward { downstream } => remove_ip_forward(downstream).await,
            Self::Ip(command) => command.delete_result(connection).await,
            // Revert is intentionally omitted because netd tracks forwarding state globally by
            // interface pair without ownership, so disabling here may tear down system-owned state.
            //
            // https://android.googlesource.com/platform/frameworks/base/+/android-5.0.0_r1/services/core/java/com/android/server/NetworkManagementService.java#1251
            // https://android.googlesource.com/platform/system/netd/+/android-5.0.0_r1/server/CommandListener.cpp#638
            // https://android.googlesource.com/platform/system/netd/+/e11b8688b1f99292ade06f89f957c1f7e76ceae9/server/TetherController.cpp#652
            // https://android.googlesource.com/platform/system/netd/+/e11b8688b1f99292ade06f89f957c1f7e76ceae9/server/TetherController.h#40
            Self::NetdNat { .. } => true,
        }
    }
}

pub(crate) struct Runtime {
    call_id: u64,
    ports: SessionPorts,
    downstream_ipv4: DownstreamIpv4,
    ipv6_nat_intercept_mode: Option<Ipv6NatInterceptMode>,
    applied: Vec<RoutingMutation>,
    connection: netlink::RequestConnection,
}

impl Runtime {
    pub(crate) async fn start(
        call_id: u64,
        config: &SessionConfig,
        downstream_ipv4: DownstreamIpv4,
        ports: SessionPorts,
        connection: netlink::RequestConnection,
    ) -> (Self, SessionPorts) {
        let mut runtime = Self {
            call_id,
            ports,
            downstream_ipv4,
            ipv6_nat_intercept_mode: None,
            applied: Vec::new(),
            connection,
        };
        let desired = runtime.desired_mutations(config).await;
        runtime.reconcile(desired).await;
        let committed = runtime.reconcile_committed_ports(config).await;
        (runtime, committed)
    }

    pub(crate) async fn replace(
        &mut self,
        previous: &SessionConfig,
        next: &SessionConfig,
        downstream_ipv4: DownstreamIpv4,
        ports: SessionPorts,
    ) -> io::Result<SessionPorts> {
        if previous.downstream != next.downstream {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "routing session downstream cannot change",
            ));
        }
        self.downstream_ipv4 = downstream_ipv4;
        self.ports = ports;
        self.reset_changed_ipv4_counter_rules(previous, next).await;
        let desired = self.desired_mutations(next).await;
        self.reconcile(desired).await;
        Ok(self.reconcile_committed_ports(next).await)
    }

    async fn reconcile_committed_ports(&mut self, config: &SessionConfig) -> SessionPorts {
        let committed = self.committed_ports(config);
        self.ports = committed.clone();
        let desired = self.desired_mutations(config).await;
        self.reconcile(desired).await;
        committed
    }

    pub(crate) fn request_connection(&mut self) -> &mut netlink::RequestConnection {
        &mut self.connection
    }

    pub(crate) async fn drain_applied(&mut self) {
        while let Some(mutation) = self.applied.pop() {
            mutation.delete(&mut self.connection).await;
        }
    }

    async fn reset_changed_ipv4_counter_rules(
        &mut self,
        previous: &SessionConfig,
        next: &SessionConfig,
    ) {
        for address in changed_ipv4_forward_counter_addresses(&previous.clients, &next.clients) {
            let Some(client) = previous
                .clients
                .iter()
                .find(|client| client.ipv4.contains(&address))
            else {
                continue;
            };
            for rule in Self::client_ip_stats_rules(previous, client.mac, address) {
                let mutation = RoutingMutation::Iptables(rule);
                let mut index = self.applied.len();
                while index > 0 {
                    index -= 1;
                    if self.applied[index] == mutation
                        && mutation.delete(&mut self.connection).await
                    {
                        self.applied.remove(index);
                    }
                }
            }
        }
    }

    async fn reconcile(&mut self, desired: Vec<RoutingMutation>) {
        let mut index = self.applied.len();
        while index > 0 {
            index -= 1;
            if !desired.contains(&self.applied[index]) {
                let mutation = self.applied[index].clone();
                if mutation.delete(&mut self.connection).await {
                    self.applied.remove(index);
                }
            }
        }
        for mutation in desired {
            if self.applied.contains(&mutation) {
                continue;
            }
            match &mutation {
                RoutingMutation::Iptables(rule) => {
                    match apply_iptables_batch(rule.target, rule.table, std::slice::from_ref(rule))
                        .await
                    {
                        Ok(()) => self.applied.push(mutation),
                        Err(e) => {
                            report::io_with_details("routing.apply_iptables", e, rule.details())
                        }
                    }
                }
                _ => match mutation.apply(&mut self.connection).await {
                    Ok(()) => {
                        if !matches!(mutation, RoutingMutation::EnsureIptablesChain { .. }) {
                            self.applied.push(mutation);
                        }
                    }
                    Err(e) => report::io("routing.apply", e),
                },
            }
        }
    }
}

pub(crate) async fn ensure_ipv6_nat_firewall_base() -> io::Result<()> {
    for chain in Ipv6NatFirewall::NAT_MANGLE_CHAINS {
        ensure_iptables_chain_result(chain.target, chain.table, chain.chain).await?;
    }
    let rules = Ipv6NatFirewall::base_rules();
    apply_iptables_batch(IptablesTarget::Ipv6, "mangle", &rules).await
}

pub(crate) async fn delete_ipv6_nat_firewall_base() {
    for rule in Ipv6NatFirewall::base_rules().into_iter().rev() {
        rule.delete().await;
    }
}

pub(crate) async fn clean(
    connection: &mut netlink::RequestConnection,
    command: &CleanRoutingCommand,
) -> io::Result<()> {
    delete_rule_repeated(
        connection,
        IpFamily::Ipv6,
        rule_priority(RULE_PRIORITY_DAEMON_BASE),
    )
    .await?;
    delete_rule_repeated(
        connection,
        IpFamily::Ipv4,
        rule_priority(RULE_PRIORITY_UPSTREAM_BASE),
    )
    .await?;
    delete_rule_repeated(
        connection,
        IpFamily::Ipv4,
        rule_priority(RULE_PRIORITY_UPSTREAM_FALLBACK_BASE),
    )
    .await?;
    delete_rule_repeated(
        connection,
        IpFamily::Ipv4,
        rule_priority(RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM_BASE),
    )
    .await?;
    clean_ip(connection, command).await?;
    firewall_cleanup::clean().await;
    Ok(())
}

fn push_unique(mutations: &mut Vec<RoutingMutation>, mutation: RoutingMutation) {
    if !mutations.contains(&mutation) {
        mutations.push(mutation);
    }
}

fn rule_priority(base: u32) -> u32 {
    rule_priority_for_api(base, platform::android_api_level())
}

fn rule_priority_for_api(base: u32, api_level: i32) -> u32 {
    (base as i32 + if api_level < 31 { -3000 } else { 0 }) as u32
}
