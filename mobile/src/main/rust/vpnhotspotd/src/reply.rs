//! One egress socket's replies, and the events it reports.
//!
//! Kept apart from the tables it serves because it shares no state with them: the task below owns nothing
//! but a reference to the socket and a channel, and every check, every allocator, and every write lives with
//! the table instead. That split is what lets a table stay lock-free in the ingress task while its sockets
//! wait on readiness of their own.
//!
//! One loop serves both relays even though what they own differs - a UDP socket per mapping, a ping socket
//! per family - because none of the subtlety here is about which transport it is. Sizing a read from the
//! queued datagram, distinguishing stale readiness from a real error, draining the error queue, and saying
//! which kind of ending this was are the same problem either way, and a second copy would be a second place
//! for them to drift.
//!
//! It waits on [ERROR_OR_READABLE] rather than on readability, and that is load-bearing rather than
//! defensive. A pending ICMP error raises `EPOLLERR` and nothing else - `datagram_poll` only adds `EPOLLIN`
//! when there is data in the receive queue - while tokio maps `Interest::READABLE` to
//! `Ready::READABLE | Ready::READ_CLOSED`, which does not include `Ready::ERROR`. Waiting on readability
//! alone therefore never wakes for an error at all: it would sit in the queue until unrelated traffic
//! happened to arrive, which is indistinguishable from the remote never having sent it.
//!
//! Nothing terminal travels on the channel below. The close is this task *finishing*, which is what
//! [crate::workers] joins, because a message saying "closed" would still be sent while this
//! task held its `Arc` of the descriptor. Every send here therefore races the token as well: a payload
//! handed to a saturated owner must not be what keeps a retirement waiting.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::workers::Ended;
use socket2::Socket;
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::icmp_translate::Reported;
use vpnhotspotd::shared::reply_bound::{
    reply_channel_footprint, Drained, ErrorSource, Turn, Turned,
};

use crate::budget::REPLY_QUEUE_DEPTH;
use crate::egress;
use crate::socket::is_kernel_icmp_error;

/// What a receive task reports while it runs.
///
/// `K` is whatever the owning table keys its sockets by, which is the mapping's TUN-visible source for UDP
/// and the family for Echo. The task never interprets it and only carries it back.
pub(crate) enum Event<K> {
    Reply {
        key: K,
        id: u64,
        remote: SocketAddr,
        hop_limit: u8,
        interface: u32,
        payload: Vec<u8>,
    },
    /// One ICMP error a router sent about traffic this socket relayed. Reported rather than drained away,
    /// because only the owner knows which client the socket serves and therefore whether the error describes
    /// state it can vouch for.
    ///
    /// Singular rather than a batch of them: how many errors are queued at once is a remote's choice, so an
    /// event carrying a vector of them would let a sender size an allocation. The queue is drained one message
    /// at a time and each is handed over on its own, through the same bounded channel as every other event.
    Error { key: K, id: u64, error: Reported },
}

/// The largest an IP datagram can be, so a read sized by this can never truncate one.
const MAX_DATAGRAM: usize = u16::MAX as usize;

/// One socket's error queue, as the shared turn sees it.
///
/// The adapter lets the shared turn borrow the scratch: a source that built its own would give this worker a
/// second ancillary buffer beside the one it already owns for its life.
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
///
/// Split from the construction below so an owner can fold this into the single reserve it takes for all its
/// fixed state and *then* build - reserve-before-allocate structurally, rather than by comment. What kept the
/// two together before was the worry that a depth spelled twice is a queue deeper than its charge; that is
/// answered by the assertion in [reply_channel] instead, which compares the built channel against this very
/// figure.
///
/// What is charged is the channel's whole retained allocation - its shared state, its value blocks and their
/// headers - plus every payload its slots may carry, because a worker takes a slot before it sizes or
/// allocates a datagram, and one payload more: the datagram the owner has already received out of a slot and
/// is still holding while that slot is refilled. See [vpnhotspotd::shared::reply_bound].
pub(crate) fn reply_channel_bytes<K>() -> Option<u64> {
    // One sender per worker, because this owner clones it into every one it starts - so the grow-race term
    // is whatever the permits allow rather than one. See [vpnhotspotd::shared::reply_bound::blocks_for].
    reply_channel_footprint::<Event<K>>(REPLY_QUEUE_DEPTH, REPLY_QUEUE_DEPTH, MAX_DATAGRAM as u64)
}

/// Builds one reply channel, at the depth [reply_channel_bytes] was charged for.
///
/// Called only after that charge has been granted. The assertion checks the channel against the figure the
/// owner reserved, so a depth changed in one place cannot silently become a queue nobody paid for.
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
    ///
    /// Required for ping sockets. `ping_recvmsg` is its own implementation and returns `min(skb->len, len)`,
    /// treating `MSG_TRUNC` as an output flag only - so a one-byte peek reports one byte rather than the
    /// datagram's length, and sizing a read from it truncates every reply to nothing.
    ///
    /// https://android.googlesource.com/kernel/common/+/refs/heads/android16-6.12/net/ipv4/ping.c
    Fixed,
}

/// One socket's replies. Deliberately does no packetization: it hands the owner exactly the datagram it
/// received, and every check, allocator and write lives with the owner.
///
/// Takes the socket by value so that returning drops this task's share of it. The owner holds the other, and
/// dropping that one after this task has been joined is what closes the descriptor.
/// What a worker waits on before it may act, so its owner can retain it before the operation it serves has
/// committed to happening.
///
/// The point is the ordering: a task that is spawned only *after* a send succeeded is a fallible step after
/// the commit point, and one that is spawned before it and left running would receive for a mapping that was
/// never published. Retained-but-gated is neither - the owner already holds the task, so the ordinary
/// join-then-release fence settles it whichever way the operation goes, and cancellation reaches it while it
/// has done nothing.
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
    // readiness would allocate as often as a remote chose to send errors, and holding one across a handover
    // would be an allocation nobody charged parked on a channel. This is charged with the record that
    // admitted this worker - see [egress::ErrorQueue::footprint].
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
                    // at least one byte, because a zero-length UDP payload is legal and still has to be
                    // consumed
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
                interface: received.interface,
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
            // The owner is gone, or a retirement asked this worker to stop.
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
