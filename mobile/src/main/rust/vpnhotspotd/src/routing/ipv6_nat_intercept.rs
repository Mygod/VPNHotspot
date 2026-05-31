use std::io;
use std::sync::OnceLock;

use futures_util::{pin_mut, TryStreamExt};
use rtnetlink::{
    packet_route::{
        route::RouteHeader,
        rule::{RuleAction as NetlinkRuleAction, RuleAttribute, RuleMessage},
        IpProtocol,
    },
    IpVersion,
};
use vpnhotspotd::shared::{
    model::{kernel_release_supports_fra_ip_proto, DAEMON_TABLE},
    protocol::{daemon_error_report_with_details, error_errno},
};

use crate::{netlink, platform, report};
use tokio::sync::Mutex;

use super::{
    netlink_commands::{
        apply_rule_command, is_missing, rule_details, IpFamily, IpOperation, IpRuleCommand,
        RuleAction,
    },
    rule_priority, RULE_PRIORITY_DAEMON_BASE,
};

const PROBE_IIF: &str = "vpnhs_probe0";
static PROBE_RESULT: OnceLock<Mutex<Option<Ipv6NatInterceptProbeResult>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Ipv6NatInterceptMode {
    ProtocolRules,
    FwmarkFallback,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Ipv6NatInterceptProbeResult {
    mode: Ipv6NatInterceptMode,
    failure: Option<String>,
    kernel_release: Option<String>,
}

impl Ipv6NatInterceptMode {
    pub(super) async fn detect(call_id: u64, handle: &netlink::Handle, downstream: &str) -> Self {
        let result = {
            let mut cached = PROBE_RESULT.get_or_init(|| Mutex::new(None)).lock().await;
            if let Some(result) = cached.as_ref() {
                result.clone()
            } else {
                let result = match probe_protocol_rules(handle).await {
                    Ok(()) => Ipv6NatInterceptProbeResult {
                        mode: Self::ProtocolRules,
                        failure: None,
                        kernel_release: None,
                    },
                    Err(reason) => Ipv6NatInterceptProbeResult {
                        mode: Self::FwmarkFallback,
                        failure: Some(reason),
                        kernel_release: platform::kernel_release().ok(),
                    },
                };
                *cached = Some(result.clone());
                result
            }
        };
        if let (Self::FwmarkFallback, Some(release), Some(reason)) = (
            result.mode,
            result.kernel_release.as_ref(),
            result.failure.as_ref(),
        ) {
            if kernel_release_supports_fra_ip_proto(release) == Some(true) {
                report::report_for(
                    Some(call_id),
                    daemon_error_report_with_details(
                        "routing.ipv6_nat_fwmark_fallback",
                        "IPv6 NAT protocol policy rule probe failed; using fwmark fallback",
                        "KernelCapabilityProbe",
                        [
                            ("downstream", downstream.to_owned()),
                            ("kernel_release", release.clone()),
                            ("probe_iif", PROBE_IIF.to_owned()),
                            (
                                "probe_priority",
                                rule_priority(RULE_PRIORITY_DAEMON_BASE).to_string(),
                            ),
                            ("probe_result", reason.clone()),
                            ("mode", "FwmarkFallback".to_owned()),
                        ],
                    ),
                );
            }
        }
        result.mode
    }
}

async fn probe_protocol_rules(handle: &netlink::Handle) -> Result<(), String> {
    let command = probe_command(Some(IpProtocol::Tcp));
    let result = match apply_rule_command(handle, &command).await {
        Ok(()) => None,
        Err(e) if error_errno(&e) == Some(libc::EEXIST) => None,
        Err(e) => Some(Err(format!("add failed: {e}"))),
    };
    let result = if let Some(result) = result {
        result
    } else {
        match dump_contains_probe_rule(handle).await {
            Ok(true) => Ok(()),
            Ok(false) => Err("probe rule dump did not echo FRA_IP_PROTO".to_owned()),
            Err(e) => Err(format!("dump failed: {e}")),
        }
    };
    cleanup_probe_rules(handle).await;
    result
}

async fn cleanup_probe_rules(handle: &netlink::Handle) {
    for mut command in [probe_command(Some(IpProtocol::Tcp)), probe_command(None)] {
        command.operation = IpOperation::Delete;
        loop {
            match apply_rule_command(handle, &command).await {
                Ok(()) => {}
                Err(e) if is_missing(&e) => break,
                Err(e) => {
                    report::io_with_details(
                        "routing.ipv6_nat_protocol_probe_cleanup",
                        e,
                        rule_details(&command),
                    );
                    break;
                }
            }
        }
    }
}

async fn dump_contains_probe_rule(handle: &netlink::Handle) -> io::Result<bool> {
    let _dump = handle.lock_dump().await;
    let rules = handle.raw().rule().get(IpVersion::V6).execute();
    pin_mut!(rules);
    while let Some(rule) = rules.try_next().await.map_err(netlink::to_io_error)? {
        if is_probe_rule(&rule) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_probe_rule(rule: &RuleMessage) -> bool {
    let table = rule
        .attributes
        .iter()
        .find_map(|attribute| {
            if let RuleAttribute::Table(table) = attribute {
                Some(*table)
            } else {
                None
            }
        })
        .or_else(|| {
            (rule.header.table != RouteHeader::RT_TABLE_UNSPEC).then_some(rule.header.table as u32)
        });
    rule.header.action == NetlinkRuleAction::ToTable
        && table == Some(DAEMON_TABLE)
        && rule
            .attributes
            .iter()
            .any(|attribute| matches!(attribute, RuleAttribute::Priority(priority) if *priority == rule_priority(RULE_PRIORITY_DAEMON_BASE)))
        && rule
            .attributes
            .iter()
            .any(|attribute| matches!(attribute, RuleAttribute::Iifname(iif) if iif == PROBE_IIF))
        && rule
            .attributes
            .iter()
            .any(|attribute| matches!(attribute, RuleAttribute::IpProtocol(IpProtocol::Tcp)))
}

fn probe_command(ip_protocol: Option<IpProtocol>) -> IpRuleCommand {
    IpRuleCommand {
        operation: IpOperation::Replace,
        family: IpFamily::Ipv6,
        iif: PROBE_IIF.to_owned(),
        priority: rule_priority(RULE_PRIORITY_DAEMON_BASE),
        action: RuleAction::Lookup,
        table: DAEMON_TABLE,
        fwmark: None,
        ip_protocol,
    }
}
