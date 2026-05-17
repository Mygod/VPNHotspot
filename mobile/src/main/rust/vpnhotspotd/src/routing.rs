use std::io;

use crate::{firewall::IptablesTarget, netlink};
use vpnhotspotd::shared::downstream::DownstreamIpv4;
use vpnhotspotd::shared::model::{SessionConfig, SessionPorts};
use vpnhotspotd::shared::proto::daemon::CleanRoutingCommand;
use vpnhotspotd::shared::protocol::IoResultReportExt;

mod desired;
mod firewall_cleanup;
mod iptables;
mod ipv6_nat_firewall;
mod ndc;
mod netlink_commands;
mod static_addresses;

use iptables::{
    apply_iptables_batch, ensure_iptables_chain, ensure_iptables_chain_result, IptablesRule,
};
use ipv6_nat_firewall::Ipv6NatFirewall;
use ndc::{add_ip_forward, remove_ip_forward, run_ndc};
use netlink_commands::{
    add_rule, apply_address, apply_route, delete_address_result, delete_route_result,
    delete_rule_repeated, delete_rule_result, IpAddressCommand, IpFamily, IpRouteCommand,
    IpRuleCommand,
};
use static_addresses::clean_ip;

pub(crate) use static_addresses::replace_static_addresses;

// AOSP local-network/tethering priorities are 20000/21000 since Android 12 and 17000/18000
// on API 29..30. Keep VPNHotspot rules inside that gap.
// This also works for Wi-Fi direct where there's no system tethering rule to override.
//
// Sources:
// https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/server/RouteController.cpp#65
// https://android.googlesource.com/platform/system/netd/+/e11b8688b1f99292ade06f89f957c1f7e76ceae9/server/RouteController.h#51
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
            } => {
                // 0 means that there are no interface addresses coming after, which is unused anyway.
                run_ndc("Nat", &["nat", "enable", downstream, upstream, "0"]).await
            }
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
    ports: SessionPorts,
    downstream_ipv4: DownstreamIpv4,
    netlink: netlink::Handle,
    applied: Vec<RoutingMutation>,
}

impl Runtime {
    pub(crate) async fn start(
        config: &SessionConfig,
        downstream_ipv4: DownstreamIpv4,
        ports: SessionPorts,
        netlink: netlink::Handle,
    ) -> io::Result<Self> {
        let mut runtime = Self {
            ports,
            downstream_ipv4,
            netlink,
            applied: Vec::new(),
        };
        if let Err(e) = runtime.setup(config).await.with_report_context_details(
            "routing.start",
            [("downstream", config.downstream.as_str())],
        ) {
            runtime.drain_applied().await;
            return Err(e);
        }
        Ok(runtime)
    }

    pub(crate) async fn replace(
        &mut self,
        previous: &SessionConfig,
        next: &SessionConfig,
        downstream_ipv4: DownstreamIpv4,
    ) -> io::Result<()> {
        if previous.downstream != next.downstream {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "routing session downstream cannot change",
            ));
        }
        self.downstream_ipv4 = downstream_ipv4;
        let desired = self.desired_mutations(next).await?;
        self.reconcile(desired).await
    }

    pub(crate) async fn stop(mut self) {
        self.drain_applied().await;
    }

    async fn setup(&mut self, config: &SessionConfig) -> io::Result<()> {
        let desired = self.desired_mutations(config).await?;
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
    handle: &netlink::Handle,
    command: &CleanRoutingCommand,
) -> io::Result<()> {
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
    firewall_cleanup::clean().await;
    Ok(())
}

fn push_unique(mutations: &mut Vec<RoutingMutation>, mutation: RoutingMutation) {
    if !mutations.contains(&mutation) {
        mutations.push(mutation);
    }
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
