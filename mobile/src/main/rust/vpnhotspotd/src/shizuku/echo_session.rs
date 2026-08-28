//! The outstanding pings, and the substituted sequence each is known by upstream.
//!
//! Apart from the relay for the same reason [crate::shizuku::echo_socket] is: this holds records and no descriptor, so it
//! is refunded the moment an entry is dropped rather than after a task reports a close. Keeping the two apart
//! means neither has to carry the other's retirement rules.
//!
//! Two lookups, and they are not interchangeable. A reply names the remote it came from, so it finds its session
//! by `(remote, sequence)` - which makes the address filter structural, because a reply from somewhere the daemon
//! never sent to looks up a key that does not exist. An *error* about a request names no remote at all, since a
//! ping socket has no ports for the kernel to report, so it can only be found by the sequence - and that lookup
//! answers "exactly one" or "more than one" rather than picking, because picking would be inventing.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use vpnhotspotd::shared::admission::logical_footprint;

use vpnhotspotd::shared::echo_wire::Identity;

/// Timers: an Echo session lives at least 60 seconds. Taken as exactly that, unlike the UDP mapping timeout
/// where a recommendation sits above the floor: nothing recommends longer for a ping, and a session is consumed
/// by its own reply anyway, so this only bounds how long an unanswered one occupies a sequence.
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
    /// How many live sessions this table may hold. Its own number rather than whatever the container rounded
    /// its request up to: the charge below is taken for this many rows, so this is what policy allows, and
    /// reading the container's capacity as the bound would let a rounding decision set a resource limit.
    prepared: usize,
    next_sequence: u16,
}

impl Sessions {
    /// Prepares for `sessions` outstanding pings: the logical maximum, which is what the charge covers and the
    /// one thing [Sessions::admits] refuses on. `with_capacity` requests it up front so the common case
    /// allocates nothing, but the container is free to reorganise its own backing from there - what the
    /// ordering needs is that a send already committed always has a *slot* to record its session in, which is
    /// the bound's job.
    pub(crate) fn with_capacity(sessions: usize) -> Self {
        Self {
            sessions: HashMap::with_capacity(sessions),
            prepared: sessions,
            next_sequence: 0,
        }
    }

    /// What a table prepared for `sessions` rows costs, whatever is in it. Retained because it was taken for
    /// the bound rather than for the rows in it, so a session being taken or expiring refunds nothing here.
    /// The container's own backing is count-bounded rather than charged - see [logical_footprint].
    pub(crate) fn footprint(sessions: usize) -> Option<u64> {
        logical_footprint::<(Key, Session)>(sessions)?
            .checked_add(std::mem::size_of::<Self>() as u64)
    }

    /// Whether one more session may be inserted: inside the prepared bound, which is the logical maximum this
    /// table was charged row state for.
    ///
    /// Asked before the request goes on the wire, because a session recorded after a successful send must have
    /// a slot to go in - see [crate::shizuku::echo::Relay]. An expiry frees one for the next ping. The map's own
    /// `capacity()` is not consulted; its backing is opaque count-bounded overhead rather than accounted
    /// state.
    pub(crate) fn admits(&self) -> bool {
        self.sessions.len() < self.prepared
    }

    /// Finds a sequence this remote has no live session under, without creating one.
    ///
    /// Separate from [Sessions::insert] so the caller can put the sequence on the wire before committing to a
    /// session: a send that fails should leave nothing behind.
    ///
    /// Monotonic rather than random, so a reused sequence is as far from its predecessor as the space allows. The
    /// walk is bounded by the space itself and stops at the first gap, so `None` means the space really is
    /// exhausted for this one remote rather than that the search gave up.
    ///
    /// Whether `None` is reachable depends on the device: the budget's ceiling is measured from
    /// `RLIMIT_NOFILE`, so on a host whose limit is below 65536 the aggregate charge runs out first and this
    /// cannot fail. It is not written as an invariant for that reason - the limit is read at runtime, and a host
    /// generous enough to allow more sessions than there are sequences would reach it.
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

    /// Drops what has timed out, returning how many went so the caller can refund them. Nothing is awaited: a
    /// session holds no descriptor, so there is no close to wait for.
    pub(crate) fn expire(&mut self, now: Instant) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|_, session| session.deadline > now);
        before - self.sessions.len()
    }

    /// Drops everything, returning how many went. Used by retirement, where every session belongs to a socket
    /// that is being closed with the generation that opened it.
    pub(crate) fn clear(&mut self) -> usize {
        let held = self.sessions.len();
        self.sessions.clear();
        held
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
