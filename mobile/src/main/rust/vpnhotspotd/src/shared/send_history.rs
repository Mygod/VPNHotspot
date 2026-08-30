//! UDP-endpoint metadata used to translate datagram-specific remote ICMP errors.
//!
//! Linux has already matched each `MSG_ERRQUEUE` item to the UDP socket and [returns the original destination]
//! that caused it. The mapping therefore retains only the hop limit used for each successfully contacted
//! endpoint; it does not retain one row or any payload per packet. The table has no count, byte, or independent
//! time limit: each IP and its endpoints use the mapping's existing RFC 4787 five-minute remote-authorization
//! deadline, and dropping the mapping releases the whole table. Allocator exhaustion has the same recoverable
//! app-UID-process failure semantics as the daemon's other uncapped, trusted-downstream memory state.
//!
//! The pinned Android kernel's error paths look up the socket from the offending UDP tuple before queueing the
//! error: IPv4 [lookup] and [enqueue], IPv6 [lookup6] and [enqueue6].
//!
//! [returns the original destination]: https://man7.org/linux/man-pages/man7/ip.7.html
//! [lookup]: https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/net/ipv4/udp.c#735
//! [enqueue]: https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/net/ipv4/udp.c#808
//! [lookup6]: https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/net/ipv6/udp.c#588
//! [enqueue6]: https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/net/ipv6/udp.c#643
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Evidence {
    Exact { hop_limit: u8 },
    Ambiguous,
}

struct Remote {
    deadline: Instant,
    endpoints: HashMap<u16, Evidence>,
}

/// What consulting the mapping's successful sends established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Every successful send to this endpoint used this hop limit.
    Matched { hop_limit: u8 },
    /// Successful sends to this endpoint used different hop limits, so a truthful quote cannot be rebuilt.
    Ambiguous,
    /// This mapping has not successfully sent to the endpoint.
    Untracked,
}

#[derive(Default)]
pub struct History {
    /// The remote-authorization deadline and endpoint evidence have one owner, so expiring an IP cannot leave
    /// stale hop limits that would poison a later authorization. Both maps grow without a quota as distinct
    /// remotes and endpoints arrive during the mapping lifetime.
    remotes: HashMap<IpAddr, Remote>,
}

impl History {
    /// Retains the hop-limit evidence for a successfully contacted endpoint. Repeated traffic using the same
    /// value leaves one exact row; observing a different value makes the endpoint permanently ambiguous for
    /// this mapping generation.
    pub fn record(&mut self, destination: SocketAddr, hop_limit: u8, deadline: Instant) {
        let remote = self
            .remotes
            .entry(destination.ip())
            .or_insert_with(|| Remote {
                deadline,
                endpoints: HashMap::new(),
            });
        remote.deadline = deadline;
        remote
            .endpoints
            .entry(destination.port())
            .and_modify(|evidence| {
                if *evidence != (Evidence::Exact { hop_limit }) {
                    *evidence = Evidence::Ambiguous;
                }
            })
            .or_insert(Evidence::Exact { hop_limit });
    }

    /// Supplies the original hop limit when this mapping can do so exactly. The kernel error queue supplies
    /// the per-datagram socket/destination correlation; this table supplies only the field it does not return.
    pub fn resolve(&self, destination: SocketAddr) -> Resolution {
        match self
            .remotes
            .get(&destination.ip())
            .and_then(|remote| remote.endpoints.get(&destination.port()))
        {
            Some(Evidence::Exact { hop_limit }) => Resolution::Matched {
                hop_limit: *hop_limit,
            },
            Some(Evidence::Ambiguous) => Resolution::Ambiguous,
            None => Resolution::Untracked,
        }
    }

    pub fn authorizes(&self, remote: IpAddr) -> bool {
        self.remotes.contains_key(&remote)
    }

    /// Drops an IP's endpoint evidence at the same caller-owned deadline as its existing reply authorization;
    /// there is no history-specific timeout.
    pub fn expire(&mut self, now: Instant) -> usize {
        let before = self.remotes.len();
        self.remotes.retain(|_, remote| remote.deadline > now);
        before - self.remotes.len()
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.remotes.values().map(|remote| remote.deadline).min()
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use super::*;
    use crate::shared::icmp_translate::{self, Correlation, Quote, Reported};
    use crate::shared::packet_writer::validate;

    fn destination(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), port)
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(300)
    }

    #[test]
    fn a_matched_send_authorizes_destination_unreachable_translation() {
        let client = destination(53_000);
        let remote = destination(443);
        let mut history = History::default();
        history.record(remote, 57, deadline());
        let error = Reported {
            remote: IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)),
            destination: Some(remote),
            hop_limit: 41,
            icmp_type: 3,
            code: 3,
            info: 0,
            quoted: Quote::default(),
        };
        let Resolution::Matched { hop_limit } = history.resolve(remote) else {
            panic!("the successful send must match");
        };
        let packet = icmp_translate::repeat(client, &error, Correlation::Datagram { hop_limit })
            .expect("the matched destination-unreachable is translatable");
        assert_eq!(validate(&packet, 1500), Ok(()));
    }

    #[test]
    fn history_grows_past_the_removed_fixed_depth() {
        let mut history = History::default();
        for port in 1..=64 {
            history.record(destination(port), 64, deadline());
        }
        for port in 1..=64 {
            assert_eq!(
                history.resolve(destination(port)),
                Resolution::Matched { hop_limit: 64 }
            );
        }
    }

    #[test]
    fn repeated_sends_with_one_hop_limit_keep_one_exact_endpoint() {
        let remote = destination(443);
        let mut history = History::default();
        history.record(remote, 57, deadline());
        history.record(remote, 57, deadline());
        assert_eq!(history.remotes[&remote.ip()].endpoints.len(), 1);
        assert_eq!(
            history.resolve(remote),
            Resolution::Matched { hop_limit: 57 }
        );
    }

    #[test]
    fn differing_hop_limits_make_the_endpoint_ambiguous() {
        let remote = destination(443);
        let mut history = History::default();
        history.record(remote, 57, deadline());
        history.record(remote, 42, deadline());
        assert_eq!(history.remotes[&remote.ip()].endpoints.len(), 1);
        assert_eq!(history.resolve(remote), Resolution::Ambiguous);
    }

    #[test]
    fn an_uncontacted_endpoint_is_not_claimed() {
        let mut history = History::default();
        history.record(destination(443), 57, deadline());
        assert_eq!(history.resolve(destination(53)), Resolution::Untracked);
    }

    #[test]
    fn remote_expiry_drops_its_endpoint_evidence_before_recontact() {
        let remote = destination(443);
        let expires = Instant::now() + Duration::from_secs(300);
        let mut history = History::default();
        history.record(remote, 57, expires);
        assert!(history.authorizes(remote.ip()));
        assert_eq!(history.expire(expires), 1);
        assert!(!history.authorizes(remote.ip()));
        assert_eq!(history.resolve(remote), Resolution::Untracked);

        history.record(remote, 42, expires + Duration::from_secs(300));
        assert_eq!(
            history.resolve(remote),
            Resolution::Matched { hop_limit: 42 }
        );
    }
}
