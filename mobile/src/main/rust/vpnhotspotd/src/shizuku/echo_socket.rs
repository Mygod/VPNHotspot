//! The ping sockets relayed Echo sends through, and their lifetime.
//!
//! Apart from the session table because the two are scoped differently and retired differently. A socket
//! belongs to a family and a selected-network generation and holds a descriptor, so its budget charge is
//! refunded when its receive task has been joined - the same protocol a UDP mapping follows. A session
//! belongs to one outstanding ping, holds nothing but records, and is refunded the moment it is dropped.
//!
//! One socket per family rather than one per session, because the kernel demultiplexes Echo Replies on the
//! identifier alone: a socket per session would buy no separation and spend a descriptor each.
//!
//! Opened on first use and then held for the whole generation, including while no session exists. There is no
//! idle timer, unlike a UDP mapping's, and the difference is what the descriptor stands for: a mapping's is a
//! client's identity and expires with it, while this one holds no client state at all. Closing it when the
//! last ping is answered would only buy back one descriptor out of tens of thousands, at the cost of a
//! close-and-reopen cycle per burst of pings.

use std::fmt::{self, Display, Formatter};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use crate::shizuku::workers::{Terminal, Workers};
use socket2::{SockAddr, Socket};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use vpnhotspotd::shared::model::Network;

use vpnhotspotd::shared::admission::{Admission, Class, Denied, Lease, Request};

use crate::report;
use crate::shizuku::budget::MAX_DATAGRAM;
use crate::shizuku::egress;
use crate::shizuku::reply::{
    receive, reply_channel, reply_channel_bytes, Event, Gate, Sizing, ERROR_OR_READABLE,
};

/// How many ping sockets can exist at once: one per family, and there are two.
const FAMILIES: usize = 2;

/// Which family's ping socket something belongs to.
///
/// Not a bool. It is the key the sockets are held under and the name that appears in a close report, and at
/// both of those a bare `true` reads as a puzzle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Family {
    V4,
    V6,
}

impl Family {
    pub(crate) fn of(address: IpAddr) -> Self {
        if address.is_ipv6() {
            Self::V6
        } else {
            Self::V4
        }
    }

    pub(crate) fn ipv6(self) -> bool {
        self == Self::V6
    }
}

impl Display for Family {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::V4 => "IPv4",
            Self::V6 => "IPv6",
        })
    }
}

/// Why a family has no socket to send on. Both are ordinary refusals rather than failures, because Echo is
/// optional independently per family: a family whose ping socket will not open is a family without Echo, and
/// the rest of the dataplane is unaffected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The aggregate budget is full, so nothing was created.
    Denied,
    /// The kernel refused the socket itself.
    OpenFailed,
}

/// One socket's grant: its record and descriptor, and the one maximum datagram its receive task may hold in
/// the reply queue at a time.
struct Held {
    socket: Arc<AsyncFd<Socket>>,
    lease: Lease,
}

pub(crate) struct Sockets {
    /// One share of each descriptor, beside the task that holds the other. A socket comes back out of here
    /// only once its task has been joined, which is what the release below is keyed to.
    sockets: Workers<Family, Held>,
    /// The table's own capacity and the reply queue's slots, charged once for the session.
    tables: Lease,
    events: mpsc::Sender<Event<Family>>,
}

impl Sockets {
    pub(crate) fn new(
        admission: &mut Admission,
    ) -> Result<(Self, mpsc::Receiver<Event<Family>>), Denied> {
        // The channel's whole allocation and every payload its slots can hold, charged before any of it
        // exists. A ping socket's own persistent receive scratch is charged separately, per socket, because it
        // exists whether or not anything is queued and is not one of these payloads.
        let bytes = Workers::<Family, Held>::footprint(FAMILIES)
            .and_then(|table| table.checked_add(reply_channel_bytes::<Family>()?))
            .ok_or(Denied::Arithmetic)?;
        let tables = admission.reserve(Request::bytes(bytes, Class::General))?;
        // Reserved above, allocated here, and in that order deliberately - see
        // [crate::shizuku::reply::reply_channel].
        let (events, receiver) = reply_channel::<Family>();
        Ok((
            Self {
                sockets: Workers::with_capacity("shizuku.echo_socket", FAMILIES),
                tables,
                events,
            },
            receiver,
        ))
    }

    /// Releases the table's own capacity, after every socket in it has been settled.
    /// Gives this owner's retained capacity back, once everything it covers is physically gone.
    ///
    /// `echoes` is the ingress task's half of the reply channel, and it has to be dropped here rather than
    /// after this returns: the lease below covers that channel's whole allocation *and* every payload its
    /// slots may hold, so releasing while the receiver still owned a queued reply would be capacity given
    /// back for memory this process was still holding. Dropping it destroys whatever it had buffered - no
    /// drain, which could only wait on senders that are already gone.
    pub(crate) fn release(self, echoes: mpsc::Receiver<Event<Family>>, admission: &mut Admission) {
        drop(self.sockets);
        drop(self.events);
        drop(echoes);
        admission.release(self.tables);
    }

    /// The socket to send this family's requests on, opening it if this is the first.
    pub(crate) fn acquire(
        &mut self,
        network: Network,
        family: Family,
        admission: &mut Admission,
    ) -> Result<Arc<AsyncFd<Socket>>, Refused> {
        if let Some(held) = self.sockets.get(&family) {
            return Ok(Arc::clone(&held.record.socket));
        }
        // The table was prepared for both families at session start, so this cannot be full; checking anyway
        // keeps the "admitted then allocated" order true rather than assumed.
        if self.sockets.admits(&family).is_err() {
            return Err(Refused::Denied);
        }
        // One record for the descriptor, plus this socket's own persistent receive scratch. A ping socket
        // will not report a datagram's length, so it holds one maximum-sized buffer for its whole life -
        // charged here, per socket, and distinct from the queued payload copies, which come out of the one
        // queue reservation above. The two are different memory: the scratch is read into, the payload is
        // copied out of it.
        let Some(bytes) = (MAX_DATAGRAM as u64).checked_add(egress::ErrorQueue::footprint()) else {
            return Err(Refused::Denied);
        };
        let Ok(lease) = admission.reserve(Request {
            records: 1,
            record_class: Class::General,
            bytes,
            byte_class: Class::General,
            ..Request::default()
        }) else {
            return Err(Refused::Denied);
        };
        let socket = match self.bind(network, family) {
            Ok(socket) => Arc::new(socket),
            Err(e) => {
                // Printed because it is once per generation at most, and because this is the shape the mode's
                // one remaining unprivileged-capability question would arrive in.
                report::io_with_details("shizuku.echo_socket", e, [("family", family.to_string())]);
                admission.release(lease);
                return Err(Refused::OpenFailed);
            }
        };
        // Issued *after* the socket exists, and under the grant that already covers both. One socket, one
        // identity, one task: an Echo socket's opaque runtime cells are the `AsyncFd` registration inside the
        // `Arc` above, this identity's cancellation node, and the task admitted below. All three are taken
        // after the grant, one per socket record, and count-bounded rather than byte-charged. There is no
        // oneshot on this path.
        let Ok(identity) = self.sockets.identity() else {
            // The socket goes before the grant that pays for its record does. Left to fall out of scope it
            // would be a descriptor still open while its capacity had already been handed back.
            drop(socket);
            admission.release(lease);
            return Err(Refused::Denied);
        };
        let admitted = self.sockets.admit(
            family,
            &identity,
            Held {
                socket: Arc::clone(&socket),
                lease,
            },
            // The task's own share of the socket, dropped by returning: joining it and dropping the record
            // above is what closes the descriptor.
            receive(
                Arc::clone(&socket),
                family,
                identity.id,
                // A ping socket will not report a datagram's length, so the read cannot be sized from a peek.
                Sizing::Fixed,
                // Already published: a ping socket is admitted for a whole generation rather than for one
                // exchange, so there is no commit for its worker to wait on.
                Gate::Open,
                self.events.clone(),
                identity.cancel.clone(),
            ),
        );
        if let Err((
            Held {
                socket: held,
                lease,
            },
            _,
        )) = admitted
        {
            // Unreachable: the capacity was checked above and this is the only admitter. Unwound anyway,
            // because the alternative is a descriptor nothing owns and a grant nothing releases. Every owner
            // of the socket goes first - the record's share, this scope's own share, and the candidate
            // identity whose cancellation node is the third of this record's runtime cells. The worker future
            // is already gone: `admit` drops it rather than spawning when it refuses.
            drop(held);
            drop(socket);
            drop(identity);
            admission.release(lease);
            return Err(Refused::Denied);
        }
        Ok(socket)
    }

    /// One ping socket with its identity bound up front. The kernel picks a free non-zero identifier for a
    /// bind to port zero, and that identifier is what it demultiplexes replies on - so binding here is what
    /// makes the socket able to receive at all, not merely tidy.
    ///
    /// https://android.googlesource.com/kernel/common/+/refs/heads/android16-6.12/net/ipv4/ping.c
    fn bind(&self, network: Network, family: Family) -> io::Result<AsyncFd<Socket>> {
        let socket = egress::open_ping(network, family.ipv6())?;
        socket.bind(&SockAddr::from(SocketAddr::new(
            if family.ipv6() {
                IpAddr::V6(Ipv6Addr::UNSPECIFIED)
            } else {
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            },
            0,
        )))?;
        // Registered for errors as well as readability, because a queued ICMP error raises only EPOLLERR and
        // the reply task would otherwise never wake for one.
        AsyncFd::with_interest(socket, ERROR_OR_READABLE)
    }

    /// Whether an event came from the socket this family currently holds, rather than from one already
    /// retired whose event was still in flight.
    pub(crate) fn current(&self, family: Family, id: u64) -> bool {
        self.sockets.current(&family, id)
    }

    /// Asks every socket's task to stop. The descriptors are not gone until each task has been joined, which
    /// is what the refund is keyed to, so nothing is removed here.
    pub(crate) fn cancel(&self) {
        self.sockets.cancel_all();
    }

    /// Whether any receive task is still running, which is what a retirement drains on.
    pub(crate) fn working(&self) -> bool {
        self.sockets.working()
    }

    /// The next socket to have finished. Selected on by the owning task, so it waits forever while no socket
    /// is open rather than answering at once.
    pub(crate) async fn finished(&mut self) -> Terminal<Family> {
        self.sockets.finished().await
    }

    /// Takes one finished socket out and refunds it, in that order: the task is complete, so dropping this
    /// share closes the descriptor and only then is the budget told. `false` means the terminal was for a
    /// socket whose family has already been reopened, which its successor must survive.
    pub(crate) fn close(&mut self, family: Family, id: u64, admission: &mut Admission) -> bool {
        match self.sockets.retire(&family, id) {
            Some(Held { socket, lease }) => {
                drop(socket);
                admission.release(lease);
                true
            }
            None => false,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.sockets.len()
    }
}
