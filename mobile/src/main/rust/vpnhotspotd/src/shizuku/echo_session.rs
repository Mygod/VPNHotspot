use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use vpnhotspotd::shared::echo_wire::Identity;

/// Bounds one unanswered translated Echo request's session metadata and rewritten sequence. RFC 5508 section
/// 3.2 REQ-2 forbids expiry below 60 seconds. On expiry the metadata and sequence are released, a late reply or
/// error is unmatched, and a later request allocates a fresh sequence on the still-owned family socket.
/// See https://www.rfc-editor.org/rfc/rfc5508.html#section-3.2.
const ECHO_TIMEOUT: Duration = Duration::from_secs(60);

/// What one outstanding request is known by upstream: who it went to, and under which substituted sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Key {
    remote: IpAddr,
    sequence: u16,
}

/// What has to be restored before a reply, or an error about the request, can go back to the client.
pub(crate) struct Session {
    /// TUN-visible, so where the reply is addressed. Never an identity of the client, same as everywhere else in
    /// this mode.
    pub(crate) client: IpAddr,
    /// The pair the client chose, neither half of which survived the trip.
    pub(crate) identity: Identity,
    /// What the client sent it with, so a quote inside an error carries its value rather than a substitute.
    pub(crate) hop_limit: u8,
    deadline: Instant,
}

/// What looking one up by sequence alone found.
pub(crate) enum Found {
    /// Exactly one, which is proof the daemon sent that request.
    One { remote: IpAddr, session: Session },
    /// More than one, so which request the error is about is not knowable.
    Ambiguous,
    /// None, so either the daemon never sent it or the session is already gone.
    Missing,
}

#[derive(Default)]
pub(crate) struct Sessions {
    sessions: HashMap<Key, Session>,
    next_sequence: u16,
}

impl Sessions {
    /// Finds a sequence this remote has no live session under, without creating one. The 65,536 attempts are
    /// exactly the complete 16-bit ICMP Echo Sequence Number space; if all are live, the new request is
    /// refused until a reply or the RFC-derived expiry releases one.
    pub(crate) fn allocate(&mut self, remote: IpAddr) -> Option<u16> {
        for _ in 0..=u16::MAX {
            let sequence = self.next_sequence;
            self.next_sequence = self.next_sequence.wrapping_add(1);
            if !self.sessions.contains_key(&Key { remote, sequence }) {
                return Some(sequence);
            }
        }
        None
    }

    pub(crate) fn insert(
        &mut self,
        remote: IpAddr,
        sequence: u16,
        client: IpAddr,
        identity: Identity,
        hop_limit: u8,
        now: Instant,
    ) {
        self.sessions.insert(
            Key { remote, sequence },
            Session {
                client,
                identity,
                hop_limit,
                deadline: now + ECHO_TIMEOUT,
            },
        );
    }

    /// Consumes the session one reply belongs to. The remote is part of the key, so a reply from somewhere never
    /// sent to cannot match rather than having to be compared against something afterwards.
    pub(crate) fn take(&mut self, remote: IpAddr, sequence: u16) -> Option<Session> {
        self.sessions.remove(&Key { remote, sequence })
    }

    /// Consumes the single session a substituted sequence names, which is the only handle an error about a
    /// request offers.
    pub(crate) fn take_by_sequence(&mut self, sequence: u16) -> Found {
        let mut matches = self.sessions.keys().filter(|key| key.sequence == sequence);
        let key = match (matches.next(), matches.next()) {
            (Some(key), None) => *key,
            (Some(_), Some(_)) => return Found::Ambiguous,
            _ => return Found::Missing,
        };
        match self.sessions.remove(&key) {
            Some(session) => Found::One {
                remote: key.remote,
                session,
            },
            None => Found::Missing,
        }
    }

    /// Drops what has timed out, returning how many went for the owner's counter. Nothing is awaited: a session
    /// holds no descriptor, so there is no close or admission refund to wait for.
    pub(crate) fn expire(&mut self, now: Instant) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|_, session| session.deadline > now);
        before - self.sessions.len()
    }

    /// Drops every memory-only session during shutdown.
    pub(crate) fn clear(&mut self) {
        self.sessions.clear();
    }

    /// The earliest deadline, which is what the owning task sleeps until. None means there is nothing to expire,
    /// not that expiry is off.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.sessions.values().map(|session| session.deadline).min()
    }

    pub(crate) fn len(&self) -> usize {
        self.sessions.len()
    }
}
