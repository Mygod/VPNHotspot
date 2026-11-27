//! Exemptions that keep NAT66 interception away from the phone's own IPv6 addresses.
//!
//! A downstream packet addressed to one of the phone's addresses is local traffic. Intercepting it
//! would hand it to the daemon's upstream proxy, which can only reach the local app from the
//! phone's own address and must send UDP replies from a transparent socket bound to the exact
//! destination, which an app already holding that port makes fail with `EADDRINUSE`. The kernel
//! delivers an exempted packet to the app directly with the client's own source address instead,
//! as it does for IPv4 hotspot clients. The address set is global, so each NAT66 session follows
//! `RTNLGRP_IPV6_IFADDR` and re-dumps on every notification, including an overrun.

use std::future::pending;
use std::io;
use std::mem::{replace, take};
use std::net::Ipv6Addr;

use rtnetlink::MulticastGroup;

use crate::firewall::IptablesTarget;
use crate::{netlink, report};
use vpnhotspotd::shared::local_ipv6::{exempt_ipv6_addresses, local_ipv6_addresses};
use vpnhotspotd::shared::model::{Ipv6NatConfig, SessionConfig};

use super::iptables::IptablesRule;
use super::ipv6_nat_firewall::Ipv6NatFirewall;
use super::{RoutingMutation, Runtime};

/// The phone's IPv6 addresses as of the latest dump, kept with the subscription that announces
/// changes to them.
pub(super) struct LocalIpv6 {
    events: netlink::EventConnection,
    pub(super) addresses: Vec<Ipv6Addr>,
}

impl LocalIpv6 {
    /// Subscribes before dumping so a change racing the dump still produces a notification. A
    /// failed dump keeps the subscription with no addresses, so the next notification retries it.
    pub(super) async fn watch(
        connection: &mut netlink::RequestConnection,
        downstream: &str,
    ) -> Option<Self> {
        let events = match netlink::EventConnection::new(&[MulticastGroup::Ipv6Ifaddr]) {
            Ok(events) => events,
            Err(e) => {
                report::io_with_details(
                    "routing.local_ipv6.events",
                    e,
                    [("downstream", downstream)],
                );
                return None;
            }
        };
        let addresses = match connection.dump_ipv6_addresses().await {
            Ok(messages) => local_ipv6_addresses(&messages),
            Err(e) => {
                report::io_with_details("routing.local_ipv6.dump", e, [("downstream", downstream)]);
                Vec::new()
            }
        };
        Some(Self { events, addresses })
    }
}

impl Runtime {
    /// Waits for the next phone IPv6 address notification, or forever when the session does not
    /// follow them. Receiving is cancel-safe, so the session owner races this against its commands
    /// and runs [`Self::refresh_local_ipv6`] outside the race, because dropping an in-flight
    /// iptables mutation would leave the kernel and the applied ledger disagreeing.
    pub(crate) async fn local_ipv6_changed(&mut self) -> io::Result<()> {
        match self.local_ipv6.as_mut() {
            Some(local_ipv6) => local_ipv6.events.next().await.map(|_| ()),
            None => pending().await,
        }
    }

    /// Re-dumps the phone's IPv6 addresses and reconciles only the address exemptions, so an
    /// address change does not re-resolve upstream interfaces or retry other failed mutations.
    pub(crate) async fn refresh_local_ipv6(
        &mut self,
        config: &SessionConfig,
        changed: io::Result<()>,
    ) {
        let Some(local_ipv6) = self.local_ipv6.as_mut() else {
            return;
        };
        let previous = match changed {
            Ok(()) => match self.connection.dump_ipv6_addresses().await {
                Ok(messages) => replace(&mut local_ipv6.addresses, local_ipv6_addresses(&messages)),
                // The previous exemptions stay until a later notification dumps successfully.
                Err(e) => {
                    report::io_with_details(
                        "routing.local_ipv6.dump",
                        e,
                        [("downstream", config.downstream.clone())],
                    );
                    return;
                }
            },
            // Without the subscription the address set can only go stale, and a stale exemption
            // would keep a departed address off its upstream path, so the exemptions go with it.
            Err(e) => {
                report::io_with_details(
                    "routing.local_ipv6.events",
                    e,
                    [("downstream", config.downstream.clone())],
                );
                let previous = take(&mut local_ipv6.addresses);
                self.local_ipv6 = None;
                previous
            }
        };
        let Some(ipv6_nat) = config.ipv6_nat.as_ref() else {
            return;
        };
        let stale = Ipv6NatFirewall::local_address_return_rules(config, ipv6_nat, &previous)
            .into_iter()
            .map(RoutingMutation::Iptables)
            .collect::<Vec<_>>();
        let mut desired = self
            .applied
            .iter()
            .filter(|mutation| !stale.contains(mutation))
            .cloned()
            .collect::<Vec<_>>();
        // The same condition under which `desired_mutations` emits the NAT66 session firewall.
        if self.ports.ipv6_nat.is_some() && self.ipv6_nat_intercept_mode.is_some() {
            desired.extend(
                Ipv6NatFirewall::local_address_return_rules(
                    config,
                    ipv6_nat,
                    self.local_addresses(),
                )
                .into_iter()
                .map(RoutingMutation::Iptables),
            );
        }
        self.reconcile(desired).await;
    }

    /// The followed phone addresses, or none when the session does not follow them.
    pub(super) fn local_addresses(&self) -> &[Ipv6Addr] {
        self.local_ipv6
            .as_ref()
            .map_or(&[], |local_ipv6| &local_ipv6.addresses)
    }
}

impl Ipv6NatFirewall {
    /// Returns each phone address from both the NAT66 input filter and interception, filter rule
    /// first. Reconciliation applies them in this order and rolls back in reverse. If only the
    /// filter return fails to apply, the base input reject refuses that address explicitly until a
    /// later reconcile retries it; the failure is reported either way.
    pub(super) fn local_address_return_rules(
        config: &SessionConfig,
        ipv6_nat: &Ipv6NatConfig,
        addresses: &[Ipv6Addr],
    ) -> Vec<IptablesRule> {
        exempt_ipv6_addresses(addresses, ipv6_nat.gateway)
            .flat_map(|address| {
                let args = vec![
                    "-i".to_owned(),
                    config.downstream.clone(),
                    "-d".to_owned(),
                    format!("{address}/128"),
                    "-j".to_owned(),
                    "RETURN".to_owned(),
                ];
                [
                    IptablesRule::new(
                        IptablesTarget::Ipv6,
                        "filter",
                        "vpnhotspot_v6_input",
                        args.clone(),
                    ),
                    IptablesRule::new(IptablesTarget::Ipv6, "mangle", "vpnhotspot_v6_tproxy", args),
                ]
            })
            .collect()
    }
}
