use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use socket2::{SockAddr, Socket};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
pub(crate) use vpnhotspotd::shared::echo_wire::Family;
use vpnhotspotd::shared::workers::{Terminal, Workers};

use vpnhotspotd::shared::egress_socket;

use crate::report;
use crate::shizuku::reply::{receive, reply_channel, Event, Gate, Sizing, ERROR_OR_READABLE};

/// Why a family has no socket to send on. Both are ordinary refusals rather than failures, because Echo is
/// optional independently per family: a family whose ping socket will not open is a family without Echo, and
/// the rest of the dataplane is unaffected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The owner could not install a new family socket.
    Unavailable,
    /// The kernel refused the socket itself.
    OpenFailed,
}

pub(crate) struct Sockets {
    /// One share of each descriptor, beside the task that holds the other. A socket comes back out of here
    /// only once its task has been joined, which is what the release below is keyed to.
    sockets: Workers<Family, Arc<AsyncFd<Socket>>>,
    events: mpsc::Sender<Event<Family>>,
}

impl Sockets {
    pub(crate) fn new() -> (Self, mpsc::Receiver<Event<Family>>) {
        let (events, receiver) = reply_channel::<Family>();
        (
            Self {
                sockets: Workers::new("shizuku.echo_socket"),
                events,
            },
            receiver,
        )
    }

    pub(crate) fn release(self, echoes: mpsc::Receiver<Event<Family>>) {
        drop(self.sockets);
        drop(self.events);
        drop(echoes);
    }

    /// The socket to send this family's requests on, opening it if this is the first.
    pub(crate) fn acquire(&mut self, family: Family) -> Result<Arc<AsyncFd<Socket>>, Refused> {
        if let Some(held) = self.sockets.get(&family) {
            return Ok(Arc::clone(&held.record));
        };
        let socket = match self.bind(family) {
            Ok(socket) => Arc::new(socket),
            Err(e) => {
                // Reported because this is the shape the mode's one remaining unprivileged-capability
                // question would arrive in.
                report::io_with_details("shizuku.echo_socket", e, [("family", family.to_string())]);
                return Err(Refused::OpenFailed);
            }
        };
        // Issued after the socket exists. One socket, one identity, one task: an Echo socket's opaque runtime
        // cells are the `AsyncFd` registration inside the `Arc` above, this identity's cancellation node, and
        // the task admitted below. There is no oneshot on this path.
        let Ok(identity) = self.sockets.identity() else {
            drop(socket);
            return Err(Refused::Unavailable);
        };
        let admitted = self.sockets.admit(
            family,
            &identity,
            Arc::clone(&socket),
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
        if let Err((held, _)) = admitted {
            // Unreachable: this owner checked that the family was absent and has not yielded. Unwind every
            // socket owner anyway; `admit` already dropped the unspawned worker future.
            drop(held);
            drop(socket);
            drop(identity);
            return Err(Refused::Unavailable);
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

    /// Takes one finished socket out. `false` means the terminal was for a socket whose family has already
    /// been reopened, which its successor must survive.
    pub(crate) fn close(&mut self, family: Family, id: u64) -> bool {
        self.sockets.retire(&family, id).is_some()
    }

    pub(crate) fn len(&self) -> usize {
        self.sockets.len()
    }
}
