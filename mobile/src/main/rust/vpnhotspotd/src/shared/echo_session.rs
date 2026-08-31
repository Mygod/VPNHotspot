//! Outstanding translated Echo requests, keyed by remote and rewritten sequence.
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use crate::shared::deadlines::Deadlines;
use crate::shared::echo_wire::{Family, Identity};

/// **Resource:** lifetime of one unanswered Echo session and rewritten sequence.
/// **Derivation:** 60 seconds, the minimum in [RFC 5508 section 3.2 REQ-2].
/// **Failure mode:** unanswered sessions otherwise retain metadata and sequence values indefinitely.
/// **Exhaustion:** expiry removes the session; late replies are unmatched and the sequence becomes reusable.
///
/// [RFC 5508 section 3.2 REQ-2]: https://www.rfc-editor.org/rfc/rfc5508.html#section-3.2
const ECHO_TIMEOUT: Duration = Duration::from_secs(60);

/// What one outstanding request is known by upstream: who it went to, and under which substituted sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Key {
    remote: IpAddr,
    sequence: u16,
}

/// What has to be restored before a reply, or an error about the request, can go back to the client.
pub struct Session {
    /// TUN-visible reply destination, not client identity.
    pub client: IpAddr,
    /// Client-selected identifier and sequence.
    pub identity: Identity,
    /// Original client hop limit.
    pub hop_limit: u8,
    deadline: Instant,
}

/// What looking one up by sequence alone found.
pub enum Found {
    /// Exactly one, which is proof the daemon sent that request.
    One { remote: IpAddr, session: Session },
    /// More than one, so which request the error is about is not knowable.
    Ambiguous,
    /// None, so either the daemon never sent it or the session is already gone.
    Missing,
}

#[derive(Default)]
pub struct Sessions {
    sessions: HashMap<Key, Session>,
    /// Sessions ordered by expiry.
    deadlines: Deadlines<Key>,
    /// Live remotes by family and rewritten sequence. Router errors quote only the sequence; this index
    /// answers exact cardinality without an Internet-triggered table scan. Family separates independent ping
    /// socket sequence spaces.
    by_sequence: HashMap<(Family, u16), HashSet<IpAddr>>,
    next_sequence: u16,
}

impl Sessions {
    /// Finds a free value in this remote's complete 16-bit sequence space.
    pub fn allocate(&mut self, remote: IpAddr) -> Option<u16> {
        for _ in 0..=u16::MAX {
            let sequence = self.next_sequence;
            self.next_sequence = self.next_sequence.wrapping_add(1);
            if !self.sessions.contains_key(&Key { remote, sequence }) {
                return Some(sequence);
            }
        }
        None
    }

    pub fn insert(
        &mut self,
        remote: IpAddr,
        sequence: u16,
        client: IpAddr,
        identity: Identity,
        hop_limit: u8,
        now: Instant,
    ) {
        let key = Key { remote, sequence };
        let deadline = now + ECHO_TIMEOUT;
        // Replacement must also replace the indexed deadline.
        let previous = self.sessions.get(&key).map(|session| session.deadline);
        self.deadlines.arm(key, previous, deadline);
        // Keep replacement idempotent in the sequence index.
        self.by_sequence
            .entry((Family::of(remote), sequence))
            .or_default()
            .insert(remote);
        self.sessions.insert(
            key,
            Session {
                client,
                identity,
                hop_limit,
                deadline,
            },
        );
    }

    /// Consumes one live session by exact remote and sequence.
    pub fn take(&mut self, remote: IpAddr, sequence: u16, now: Instant) -> Option<Session> {
        self.consume(Key { remote, sequence }, now)
    }

    /// Consumes the unique live session named by family and sequence. Due rows are expired first so stale
    /// sessions cannot create false ambiguity; the returned count contributes to the ordinary sweep total.
    pub fn take_by_sequence(
        &mut self,
        family: Family,
        sequence: u16,
        now: Instant,
    ) -> (usize, Found) {
        let expired = self.expire(now);
        let found = 'found: {
            let Some(remotes) = self.by_sequence.get(&(family, sequence)) else {
                break 'found Found::Missing;
            };
            // More than one candidate is ambiguous and consumes nothing.
            let mut held = remotes.iter();
            let remote = match (held.next(), held.next()) {
                (Some(remote), None) => *remote,
                (Some(_), Some(_)) => break 'found Found::Ambiguous,
                _ => break 'found Found::Missing,
            };
            match self.consume(Key { remote, sequence }, now) {
                Some(session) => Found::One { remote, session },
                None => Found::Missing,
            }
        };
        (expired, found)
    }

    /// Removes one session, returning it only before its deadline.
    fn consume(&mut self, key: Key, now: Instant) -> Option<Session> {
        let session = self.remove(key)?;
        (now < session.deadline).then_some(session)
    }

    /// Removes one session and its deadline entry together, which is the only way either leaves this table.
    fn remove(&mut self, key: Key) -> Option<Session> {
        let session = self.drop_row(key)?;
        self.deadlines.disarm(key, session.deadline);
        Some(session)
    }

    /// Removes one session and its sequence-index entry.
    fn drop_row(&mut self, key: Key) -> Option<Session> {
        let session = self.sessions.remove(&key)?;
        let indexed = (Family::of(key.remote), key.sequence);
        if let Some(remotes) = self.by_sequence.get_mut(&indexed) {
            remotes.remove(&key.remote);
            if remotes.is_empty() {
                self.by_sequence.remove(&indexed);
            }
        }
        Some(session)
    }

    /// Test-only full check that the sequence index matches the session table.
    #[cfg(test)]
    fn indexed(&self) -> bool {
        self.by_sequence
            .values()
            .map(|remotes| remotes.len())
            .sum::<usize>()
            == self.sessions.len()
            && self
                .by_sequence
                .iter()
                .all(|((family, sequence), remotes)| {
                    !remotes.is_empty()
                        && remotes.iter().all(|remote| {
                            Family::of(*remote) == *family
                                && self.sessions.contains_key(&Key {
                                    remote: *remote,
                                    sequence: *sequence,
                                })
                        })
                })
    }

    /// Removes due sessions and returns the count.
    pub fn expire(&mut self, now: Instant) -> usize {
        let mut expired = 0;
        while let Some(key) = self.deadlines.due(now) {
            // `due` already removed the deadline entry.
            if self.drop_row(key).is_some() {
                expired += 1;
            }
        }
        expired
    }

    /// Drops every memory-only session during shutdown.
    pub fn clear(&mut self) {
        self.sessions.clear();
        self.deadlines.clear();
        self.by_sequence.clear();
    }

    /// The earliest session deadline.
    pub fn next_deadline(&self) -> Option<Instant> {
        debug_assert_eq!(
            self.deadlines.len(),
            self.sessions.len(),
            "every live session is armed exactly once"
        );
        self.deadlines.next()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 43, 10));

    fn remote(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(198, 51, 100, last))
    }

    fn remote6(last: u16) -> IpAddr {
        IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, last))
    }

    fn identity() -> Identity {
        Identity {
            identifier: 0x1234,
            sequence: 7,
        }
    }

    fn holding(rows: &[(u8, u16, u64)], now: Instant) -> Sessions {
        let mut sessions = Sessions::default();
        for (last, sequence, offset) in rows {
            sessions.insert(
                remote(*last),
                *sequence,
                CLIENT,
                identity(),
                64,
                now + Duration::from_secs(*offset),
            );
            // Avoid a production O(N) assertion on remote-driven insertion.
            assert!(
                sessions.indexed(),
                "an insertion left the index disagreeing"
            );
        }
        sessions
    }

    #[test]
    fn an_empty_table_has_nothing_to_wait_for() {
        let mut sessions = Sessions::default();
        assert!(sessions.is_empty());
        assert_eq!(sessions.next_deadline(), None);
        assert_eq!(sessions.expire(Instant::now()), 0);
    }

    #[test]
    fn the_earliest_session_is_the_one_waited_for_and_only_due_rows_expire() {
        let now = Instant::now();
        let mut sessions = holding(&[(1, 10, 30), (2, 11, 0), (3, 12, 15)], now);
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions.next_deadline(), Some(now + ECHO_TIMEOUT));

        assert_eq!(
            sessions.expire(now + ECHO_TIMEOUT - Duration::from_millis(1)),
            0
        );
        assert_eq!(sessions.expire(now + ECHO_TIMEOUT), 1);
        assert_eq!(
            sessions.next_deadline(),
            Some(now + ECHO_TIMEOUT + Duration::from_secs(15))
        );
        assert_eq!(
            sessions.expire(now + ECHO_TIMEOUT + Duration::from_secs(30)),
            2
        );
        assert_eq!(sessions.next_deadline(), None);
        assert!(sessions.is_empty());
    }

    #[test]
    fn a_reply_removes_the_session_and_its_deadline_together() {
        let now = Instant::now();
        let mut sessions = holding(&[(1, 10, 0), (2, 11, 20)], now);
        assert!(sessions.take(remote(1), 10, now).is_some());
        assert_eq!(
            sessions.next_deadline(),
            Some(now + ECHO_TIMEOUT + Duration::from_secs(20)),
            "the answered session is not still waited for"
        );
        assert!(
            sessions.take(remote(1), 10, now).is_none(),
            "and a duplicate reply matches nothing"
        );
        assert_eq!(sessions.expire(now + ECHO_TIMEOUT), 0);
    }

    #[test]
    fn an_error_matched_by_sequence_removes_the_session_and_its_deadline() {
        let now = Instant::now();
        let mut sessions = holding(&[(1, 10, 0), (2, 11, 20)], now);
        let Found::One {
            remote: matched, ..
        } = sessions.take_by_sequence(Family::V4, 10, now).1
        else {
            panic!("exactly one session uses that sequence");
        };
        assert_eq!(matched, remote(1));
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions.next_deadline(),
            Some(now + ECHO_TIMEOUT + Duration::from_secs(20))
        );
        assert!(
            matches!(
                sessions.take_by_sequence(Family::V4, 10, now).1,
                Found::Missing
            ),
            "the consumed sequence names nothing, so the index kept no stale entry"
        );
        assert!(sessions.indexed());
    }

    #[test]
    fn one_sequence_shared_by_two_remotes_names_neither_until_one_goes() {
        let now = Instant::now();
        let mut sessions = holding(&[(1, 10, 0), (2, 10, 20)], now);
        assert!(matches!(
            sessions.take_by_sequence(Family::V4, 10, now).1,
            Found::Ambiguous
        ));
        assert_eq!(sessions.len(), 2, "an ambiguous error consumes nothing");
        assert!(sessions.indexed());

        // Removing one candidate makes the other unique.
        assert!(sessions.take(remote(1), 10, now).is_some());
        assert!(sessions.indexed());
        let Found::One {
            remote: matched, ..
        } = sessions.take_by_sequence(Family::V4, 10, now).1
        else {
            panic!("one remote is left under that sequence");
        };
        assert_eq!(matched, remote(2));
        assert!(matches!(
            sessions.take_by_sequence(Family::V4, 10, now).1,
            Found::Missing
        ));
        assert!(sessions.is_empty());
        assert!(sessions.indexed());
    }

    #[test]
    fn expiry_and_clear_leave_no_sequence_naming_a_gone_session() {
        let now = Instant::now();
        let mut sessions = holding(&[(1, 10, 0), (2, 10, 30), (3, 11, 30)], now);
        assert_eq!(sessions.expire(now + ECHO_TIMEOUT), 1);
        assert!(sessions.indexed());
        let Found::One {
            remote: matched, ..
        } = sessions.take_by_sequence(Family::V4, 10, now).1
        else {
            panic!("expiry left exactly one remote under that sequence");
        };
        assert_eq!(matched, remote(2));

        let mut sessions = holding(&[(1, 10, 0), (2, 11, 0)], now);
        assert_eq!(sessions.expire(now + ECHO_TIMEOUT), 2);
        assert!(matches!(
            sessions.take_by_sequence(Family::V4, 10, now).1,
            Found::Missing
        ));
        assert!(matches!(
            sessions.take_by_sequence(Family::V4, 11, now).1,
            Found::Missing
        ));
        assert!(sessions.indexed());

        let mut sessions = holding(&[(1, 10, 0), (2, 11, 0)], now);
        sessions.clear();
        assert!(matches!(
            sessions.take_by_sequence(Family::V4, 10, now).1,
            Found::Missing
        ));
        assert!(sessions.indexed());
    }

    #[test]
    fn a_timed_out_session_is_not_restored_as_a_live_reply() {
        let now = Instant::now();
        let mut sessions = holding(&[(1, 10, 0), (2, 11, 0)], now);
        let expired = now + ECHO_TIMEOUT;
        // Point-of-use expiry must not await the sweep arm.
        assert!(
            sessions.take(remote(1), 10, expired).is_none(),
            "the deadline decides, not whether the sweep has run"
        );
        assert!(
            sessions.take(remote(1), 10, now).is_none(),
            "and the row went with the refusal, exactly as its deadline said it would"
        );
        assert!(sessions.indexed());

        // Sequence lookup applies the same expiry.
        let (swept, found) = sessions.take_by_sequence(Family::V4, 11, expired);
        assert_eq!(swept, 1);
        assert!(
            matches!(found, Found::Missing),
            "an error quoting a timed-out request matches nothing either"
        );
        assert!(sessions.is_empty());
        assert!(sessions.indexed());
        assert_eq!(
            sessions.next_deadline(),
            None,
            "and neither is still waited for"
        );
    }

    #[test]
    fn a_timed_out_remote_does_not_make_a_live_one_sharing_its_sequence_ambiguous() {
        let now = Instant::now();
        // An expired candidate must not make a live one ambiguous.
        let mut sessions = holding(&[(1, 10, 0), (2, 10, 60)], now);
        let expired = now + ECHO_TIMEOUT;
        let (swept, found) = sessions.take_by_sequence(Family::V4, 10, expired);
        assert_eq!(swept, 1, "exactly the row that was really due");
        let Found::One {
            remote: matched, ..
        } = found
        else {
            panic!("the live request is the only candidate left");
        };
        assert_eq!(matched, remote(2));
        assert!(sessions.is_empty());
        assert!(sessions.indexed());

        // Two expired candidates leave no match.
        let mut sessions = holding(&[(1, 10, 0), (2, 10, 0)], now);
        let (swept, found) = sessions.take_by_sequence(Family::V4, 10, expired);
        assert_eq!(swept, 2);
        assert!(matches!(found, Found::Missing));
        assert!(sessions.is_empty());
        assert!(sessions.indexed());

        // Two live candidates remain ambiguous.
        let mut sessions = holding(&[(1, 10, 0), (2, 10, 0)], now);
        let (swept, found) = sessions.take_by_sequence(Family::V4, 10, now);
        assert_eq!(swept, 0);
        assert!(matches!(found, Found::Ambiguous));
        assert_eq!(sessions.len(), 2, "an ambiguous error consumes nothing");
        assert!(sessions.indexed());
    }

    #[test]
    fn a_session_is_still_live_on_the_last_instant_before_its_deadline() {
        let now = Instant::now();
        let mut sessions = holding(&[(1, 10, 0)], now);
        let live = now + ECHO_TIMEOUT - Duration::from_nanos(1);
        assert_eq!(
            sessions.expire(live),
            0,
            "the sweep would not have taken it"
        );
        assert!(
            sessions.take(remote(1), 10, live).is_some(),
            "so neither does the reply path"
        );
    }

    #[test]
    fn the_two_families_hold_one_sequence_independently() {
        let now = Instant::now();
        let mut sessions = Sessions::default();
        // Ping families allocate independently.
        sessions.insert(remote(1), 10, CLIENT, identity(), 64, now);
        sessions.insert(remote6(1), 10, CLIENT, identity(), 64, now);
        assert!(sessions.indexed());
        assert_eq!(sessions.len(), 2);

        // The socket family disambiguates them.
        let Found::One {
            remote: matched, ..
        } = sessions.take_by_sequence(Family::V6, 10, now).1
        else {
            panic!("exactly one IPv6 session uses that sequence");
        };
        assert_eq!(matched, remote6(1));
        assert!(sessions.indexed());

        let Found::One {
            remote: matched, ..
        } = sessions.take_by_sequence(Family::V4, 10, now).1
        else {
            panic!("the IPv4 session was never a candidate for the IPv6 error");
        };
        assert_eq!(matched, remote(1));
        assert!(sessions.is_empty());
        assert!(sessions.indexed());
    }

    #[test]
    fn an_opposite_family_error_cannot_consume_the_only_live_session() {
        let now = Instant::now();
        let mut sessions = Sessions::default();
        sessions.insert(remote(1), 10, CLIENT, identity(), 64, now);
        assert!(
            matches!(
                sessions.take_by_sequence(Family::V6, 10, now).1,
                Found::Missing
            ),
            "an IPv6 error names no IPv4 session, whatever it quotes"
        );
        assert_eq!(
            sessions.len(),
            1,
            "and consumes nothing, so the reply it belongs to can still be matched"
        );
        assert!(sessions.indexed());
        assert!(sessions.take(remote(1), 10, now).is_some());
    }

    #[test]
    fn ambiguity_is_only_ever_among_remotes_of_one_family() {
        let now = Instant::now();
        let mut sessions = Sessions::default();
        // Two same-family remotes are ambiguous.
        sessions.insert(remote(1), 10, CLIENT, identity(), 64, now);
        sessions.insert(remote(2), 10, CLIENT, identity(), 64, now);
        // The other family is not a candidate.
        sessions.insert(remote6(1), 10, CLIENT, identity(), 64, now);
        assert!(sessions.indexed());
        assert!(matches!(
            sessions.take_by_sequence(Family::V4, 10, now).1,
            Found::Ambiguous
        ));
        let Found::One {
            remote: matched, ..
        } = sessions.take_by_sequence(Family::V6, 10, now).1
        else {
            panic!("the IPv6 side has exactly one");
        };
        assert_eq!(matched, remote6(1));
        assert_eq!(sessions.len(), 2, "the ambiguous pair is untouched");
        assert!(sessions.indexed());
    }

    #[test]
    fn a_reply_removes_the_sequence_entry_with_the_session() {
        let now = Instant::now();
        let mut sessions = holding(&[(1, 10, 0)], now);
        assert!(sessions.take(remote(1), 10, now).is_some());
        assert!(
            matches!(
                sessions.take_by_sequence(Family::V4, 10, now).1,
                Found::Missing
            ),
            "an error about an answered request matches nothing"
        );
        assert!(sessions.indexed());
    }

    #[test]
    fn clearing_releases_the_sessions_and_the_index_together() {
        let now = Instant::now();
        let mut sessions = holding(&[(1, 10, 0), (2, 11, 20)], now);
        sessions.clear();
        assert!(sessions.is_empty());
        assert_eq!(sessions.next_deadline(), None);
    }

    #[test]
    fn a_sequence_is_reused_only_once_its_session_is_gone() {
        let now = Instant::now();
        let mut sessions = Sessions::default();
        let first = sessions.allocate(remote(1)).expect("a fresh table");
        sessions.insert(remote(1), first, CLIENT, identity(), 64, now);
        assert!(sessions.indexed());
        assert_ne!(
            sessions.allocate(remote(1)).expect("another value"),
            first,
            "a live session holds its own sequence"
        );
        assert_eq!(
            sessions.allocate(remote(2)).expect("another remote"),
            first.wrapping_add(2),
            "the sequence space is shared, but a live session only blocks its own remote"
        );
    }

    #[test]
    fn inserting_over_a_live_key_leaves_one_indexed_session() {
        // Replacing a key must not duplicate its sequence-index entry.
        let now = Instant::now();
        let mut sessions = holding(&[(1, 10, 0)], now);
        sessions.insert(
            remote(1),
            10,
            CLIENT,
            identity(),
            64,
            now + Duration::from_secs(5),
        );
        assert!(sessions.indexed());
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions.next_deadline(),
            Some(now + Duration::from_secs(5) + ECHO_TIMEOUT),
            "and the deadline index replaced its entry rather than adding one"
        );
        let Found::One {
            remote: matched, ..
        } = sessions.take_by_sequence(Family::V4, 10, now).1
        else {
            panic!("one session, counted once");
        };
        assert_eq!(matched, remote(1));
        assert!(sessions.is_empty());
        assert!(sessions.indexed());
    }
}
