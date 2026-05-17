use crate::{
    firewall::{self, IptablesTarget},
    report,
};

use super::iptables::delete_iptables_repeated;
use super::ipv6_nat_firewall::Ipv6NatFirewall;
use super::Runtime;

// Clean uses iptables-restore through `firewall::restore`. Android 10's bundled
// iptables-restore supports `-w --noflush`, and with `--noflush`, `:chain - [0:0]`
// flushes an existing user chain or creates a missing one before the following `-X chain`.
//
// Sources:
// https://android.googlesource.com/platform/external/iptables/+/android-10.0.0_r1/iptables/iptables-restore.c#33
// https://android.googlesource.com/platform/external/iptables/+/android-10.0.0_r1/iptables/iptables-restore.c#354
// https://android.googlesource.com/platform/external/iptables/+/android-10.0.0_r1/iptables/ip6tables-restore.c#36
// https://android.googlesource.com/platform/external/iptables/+/android-10.0.0_r1/iptables/ip6tables-restore.c#36
// https://android.googlesource.com/platform/external/iptables/+/android-10.0.0_r1/iptables/ip6tables-restore.c#355
pub(super) async fn clean() {
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
    for rule in Runtime::ipv6_block_jump_rules()
        .into_iter()
        .chain(Ipv6NatFirewall::filter_jump_rules())
        .chain(std::iter::once(Ipv6NatFirewall::prerouting_rule()))
    {
        rule.delete_repeated().await;
    }
    let mut ip6tables_clean_input = Ipv6NatFirewall::clean_filter_input();
    ip6tables_clean_input.push_str(&Ipv6NatFirewall::clean_mangle_input());
    if let Err(e) = firewall::restore(IptablesTarget::Ipv6, &ip6tables_clean_input).await {
        report::io("routing.clean_firewall.ip6tables_restore", e);
    }
}
