use std::net::IpAddr;

use vpnhotspotd::shared::icmp_error::{self, Reason};

use crate::report;

pub(crate) struct Gateways(Vec<IpAddr>);

impl Gateways {
    pub(crate) fn new(addresses: Vec<IpAddr>) -> Self {
        Self(addresses)
    }

    /// Builds one ICMP error about `packet`, or `None` when there is nothing truthful to send.
    pub(crate) fn report(&self, packet: &[u8], reason: Reason) -> Option<Vec<u8>> {
        let ipv6 = match packet.first().map(|byte| byte >> 4) {
            Some(4) => false,
            Some(6) => true,
            _ => return None,
        };
        let source = self
            .0
            .iter()
            .copied()
            .find(|address| address.is_ipv6() == ipv6)?;
        match icmp_error::build(source, packet, reason) {
            Ok(error) => Some(error),
            Err(e) => {
                // Not reachable from any input: the family has just been checked, the packet parsed far
                // enough to be relayed, and the only other refusal is a path MTU too large to be one. So it
                // means a bug rather than a hostile packet, and is printed rather than only counted.
                report::message_with_details(
                    "shizuku.icmp_origin",
                    format!("cannot build an ICMP error: {e:?}"),
                    "packetization",
                    [
                        ("gateway", source.to_string()),
                        ("reason", format!("{reason:?}")),
                    ],
                );
                None
            }
        }
    }
}
