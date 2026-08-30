//! Reads socket replies and exceptional readiness with cancellation.
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use socket2::Socket;
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::icmp_translate::Reported;
use vpnhotspotd::shared::reply_bound::{
    reply_channel_footprint, Drained, ErrorSource, Turn, Turned,
};
use vpnhotspotd::shared::workers::Ended;

use crate::shizuku::budget::REPLY_QUEUE_DEPTH;
use crate::shizuku::egress;
use crate::socket::is_kernel_icmp_error;

/// What a receive task reports while it runs.
pub(crate) enum Event<K> {
    Reply {
        key: K,
        id: u64,
        remote: SocketAddr,
        hop_limit: u8,
        payload: Vec<u8>,
    },
    /// One ICMP error a router sent about traffic this socket relayed. Reported rather than drained away,
    /// because only the owner knows which client the socket serves and therefore whether the error describes
    /// state it can vouch for.
    Error { key: K, id: u64, error: Reported },
}

/// The largest an IP datagram can be, so a read sized by this can never truncate one.
const MAX_DATAGRAM: usize = u16::MAX as usize;

/// One socket's error queue, as the shared turn sees it.
struct Bound<'a> {
    queue: &'a mut egress::ErrorQueue,
    socket: &'a Socket,
    /// The router error the last turn took, if it took one. Kept here rather than in the classification,
    /// because the shared decision is about the slot and not about the payload.
    found: Option<Reported>,
}

impl ErrorSource for Bound<'_> {
    fn next(&mut self) -> io::Result<Drained> {
        Ok(match self.queue.next(self.socket)? {
            None => Drained::Empty,
            Some(egress::Drained::Remote(error)) => {
                self.found = Some(error);
                Drained::Remote
            }
            // A local refusal belongs to the send that provoked it, which the owner's send path reads for
            // itself; this task saw no send and has nothing to attribute it to.
            Some(egress::Drained::Local(_)) => Drained::Local,
            Some(egress::Drained::Neither) => Drained::Neither,
        })
    }
}

/// One reply channel: the sender the owner clones per worker, and the receiver it keeps.
pub(crate) type ReplyChannel<K> = (mpsc::Sender<Event<K>>, mpsc::Receiver<Event<K>>);

/// What one reply channel costs, before one exists.
pub(crate) fn reply_channel_bytes<K>() -> Option<u64> {
    // One sender per worker, because this owner clones it into every one it starts - so the grow-race term
    // is whatever the permits allow rather than one. See [vpnhotspotd::shared::reply_bound::blocks_for].
    reply_channel_footprint::<Event<K>>(REPLY_QUEUE_DEPTH, REPLY_QUEUE_DEPTH, MAX_DATAGRAM as u64)
}

/// Builds one reply channel, at the depth [reply_channel_bytes] was charged for.
pub(crate) fn reply_channel<K>() -> ReplyChannel<K> {
    let (sender, receiver) = mpsc::channel(REPLY_QUEUE_DEPTH);
    debug_assert_eq!(
        sender.max_capacity(),
        REPLY_QUEUE_DEPTH,
        "a reply channel is built at the depth it was charged for"
    );
    (sender, receiver)
}

/// What the task waits for: a datagram to forward, or an error to translate. Both, because they are separate
/// readiness bits and an error never sets the readable one - see the module note.
pub(crate) const ERROR_OR_READABLE: Interest = Interest::READABLE.add(Interest::ERROR);

/// How the next datagram's size is decided before it is read, which is not a preference: one of these is
/// wrong per socket kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sizing {
    /// Ask the socket and allocate exactly that. Correct only where `recvmsg` reports the true datagram length
    /// under `MSG_TRUNC`, which UDP does.
    Peek,
    /// Hold one buffer big enough for any datagram, for the whole life of the task.
    Fixed,
}

/// One socket's replies. Deliberately does no packetization: it hands the owner exactly the datagram it
/// received, and every check, allocator and write lives with the owner.
pub(crate) enum Gate {
    /// Nothing to wait for; the record was already published.
    Open,
    /// Wait for the owner to say the operation committed. The sender being dropped is a rollback.
    Pending(oneshot::Receiver<()>),
}

pub(crate) async fn receive<K: Copy>(
    socket: Arc<AsyncFd<Socket>>,
    key: K,
    id: u64,
    sizing: Sizing,
    gate: Gate,
    events: mpsc::Sender<Event<K>>,
    cancel: CancellationToken,
) -> Ended {
    if let Gate::Pending(commit) = gate {
        // Nothing is read, allocated or reported before this: until the owner commits, this worker holds only
        // its share of the socket, which is exactly what the rollback path needs it to hold.
        let committed = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ended::Expected,
            committed = commit => committed,
        };
        if committed.is_err() {
            // The owner dropped the gate without committing, which is a rollback that raced the cancellation.
            return Ended::Expected;
        }
    }
    // Allocated once for the task rather than per datagram, and only where the socket will not report a
    // length. That distinction is what keeps this affordable: there are two ping sockets per session, while one
    // of these per UDP mapping would be tens of thousands of them.
    let mut buffer = match sizing {
        Sizing::Peek => Vec::new(),
        Sizing::Fixed => vec![0u8; MAX_DATAGRAM],
    };
    // One per worker, for the worker's life. Its ancillary buffer is heap-backed, so building one per
    // readiness would allocate as often as a remote chose to send errors. This is charged with the record
    // that admitted this worker - see [egress::ErrorQueue::footprint].
    let mut errors = egress::ErrorQueue::new();
    loop {
        // One turn, and the whole ordering is inside it: readiness, then the slot, then the read - and only
        // then an allocation. See [vpnhotspotd::shared::reply_bound::Turn::run] for why readiness cannot come
        // second and the read cannot come first.
        let mut source = Bound {
            queue: &mut errors,
            socket: socket.get_ref(),
            found: None,
        };
        let turned = Turn {
            sender: &events,
            cancel: &cancel,
            fd: &socket,
            interest: ERROR_OR_READABLE,
            errors: &mut source,
        }
        .run(
            |inner| match sizing {
                Sizing::Peek => {
                    let length = egress::peek_length(inner)?;
                    // A zero-length UDP payload still requires one receive operation.
                    let mut payload = vec![0u8; length.max(1)];
                    let received = egress::receive(inner, &mut payload)?;
                    payload.truncate(received.bytes);
                    Ok((received, payload))
                }
                Sizing::Fixed => {
                    let received = egress::receive(inner, &mut buffer)?;
                    Ok((received, buffer[..received.bytes].to_vec()))
                }
            },
            is_kernel_icmp_error,
            |(received, payload)| Event::Reply {
                key,
                id,
                remote: received.source,
                hop_limit: received.hop_limit,
                payload,
            },
            |source| {
                source
                    .found
                    .take()
                    .map(|error| Event::Error { key, id, error })
            },
        )
        .await;
        match turned {
            // The socket stays ready while anything remains, so the next turn takes a fresh slot.
            Turned::Sent | Turned::Reported | Turned::Released => continue,
            // The owner is gone, or cancellation asked this worker to stop.
            Turned::Cancelled | Turned::Closed => return Ended::Expected,
            Turned::Failed(error) => {
                return Ended::Failed {
                    context: "shizuku.reply_receive",
                    error,
                }
            }
        }
    }
}
