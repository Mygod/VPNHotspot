use std::fmt::{self, Display, Formatter};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use socket2::{SockAddr, Socket};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use vpnhotspotd::shared::workers::{Terminal, Workers};

use vpnhotspotd::shared::admission::{Admission, Class, Lease};
use vpnhotspotd::shared::egress_socket;

use crate::report;
use crate::shizuku::reply::{receive, reply_channel, Event, Gate, Sizing, ERROR_OR_READABLE};

/// Which family's ping socket something belongs to.
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
    /// Descriptor admission is full, so no socket was created.
    Denied,
    /// The kernel refused the socket itself.
    OpenFailed,
}

/// One family socket and the descriptor grant released only after its receive task is joined.
struct Held {
    socket: Arc<AsyncFd<Socket>>,
    lease: Lease,
}

pub(crate) struct Sockets {
    /// One share of each descriptor, beside the task that holds the other. A socket comes back out of here
    /// only once its task has been joined, which is what the release below is keyed to.
    sockets: Workers<Family, Held>,
    events: mpsc::UnboundedSender<Event<Family>>,
}

impl Sockets {
    pub(crate) fn new() -> (Self, mpsc::UnboundedReceiver<Event<Family>>) {
        let (events, receiver) = reply_channel::<Family>();
        (
            Self {
                sockets: Workers::new("shizuku.echo_socket"),
                events,
            },
            receiver,
        )
    }

    pub(crate) fn release(self, echoes: mpsc::UnboundedReceiver<Event<Family>>) {
        drop(self.sockets);
        drop(self.events);
        drop(echoes);
    }

    /// The socket to send this family's requests on, opening it if this is the first.
    pub(crate) fn acquire(
        &mut self,
        family: Family,
        admission: &mut Admission,
    ) -> Result<Arc<AsyncFd<Socket>>, Refused> {
        if let Some(held) = self.sockets.get(&family) {
            return Ok(Arc::clone(&held.record.socket));
        }
        // A duplicate family is checked before taking a descriptor grant or opening anything.
        if self.sockets.admits(&family).is_err() {
            return Err(Refused::Denied);
        }
        let Ok(lease) = admission.reserve(Class::General) else {
            return Err(Refused::Denied);
        };
        let socket = match self.bind(family) {
            Ok(socket) => Arc::new(socket),
            Err(e) => {
                // Reported because this is the shape the mode's one remaining unprivileged-capability
                // question would arrive in.
                report::io_with_details("shizuku.echo_socket", e, [("family", family.to_string())]);
                admission.release(lease);
                return Err(Refused::OpenFailed);
            }
        };
        // Issued *after* the socket exists, under its one-descriptor grant. One socket, one identity, one
        // task: an Echo socket's opaque runtime cells are the `AsyncFd` registration inside the `Arc` above,
        // this identity's cancellation node, and the task admitted below. There is no oneshot on this path.
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
                // Already published: a ping socket is admitted for the whole session rather than for one
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
    fn bind(&self, family: Family) -> io::Result<AsyncFd<Socket>> {
        let socket = egress_socket::open_ping(family.ipv6())?;
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
    /// replaced whose event was still in flight.
    pub(crate) fn current(&self, family: Family, id: u64) -> bool {
        self.sockets.current(&family, id)
    }

    /// Asks every socket's task to stop. The descriptors are not gone until each task has been joined, which
    /// is what the refund is keyed to, so nothing is removed here.
    pub(crate) fn cancel(&self) {
        self.sockets.cancel_all();
    }

    /// Whether any receive task is still running, which is what shutdown drains on.
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
