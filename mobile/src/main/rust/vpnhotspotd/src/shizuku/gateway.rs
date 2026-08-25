//! The interface's own addresses, and the single thing they are for.
//!
//! An ICMP error the daemon originates has to come from an address a router in its position would speak
//! from, which is the TUN's own address for the offending packet's family - not the client's view of
//! anything, and not the destination it aimed at. Both relays that can refuse a packet need that, so the
//! set and the decision live here rather than once per transport.
//!
//! A missing address of the right family means the error is not sent at all. Substituting the other
//! family's, or the destination's, would produce something the client's stack either discards or - worse -
//! believes, so silence is the only honest alternative.

use std::net::IpAddr;

use vpnhotspotd::shared::icmp_error::{self, Reason};

use crate::report;

pub(crate) struct Gateways(Vec<IpAddr>);

impl Gateways {
    pub(crate) fn new() -> Self {
        Self(Vec::new())
    }

    pub(crate) fn set(&mut self, addresses: &[IpAddr]) {
        self.0.clear();
        self.0.extend_from_slice(addresses);
    }

    /// Builds one ICMP error about `packet`, or `None` when there is nothing truthful to send.
    ///
    /// The family comes from the packet rather than from the caller, because the caller's idea of it and the
    /// bytes on the interface are two different things and only one of them is what the client will parse.
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
