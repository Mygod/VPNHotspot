//! One terminated TCP flow's upstream half: the selected-network socket, and the splice between it and the
//! client-side stack.
//!
//! Backpressure is the point of this shape, and it is bounded in both directions without a byte counter
//! anywhere. Two things do that bounding, and they are not the same thing. A channel's depth bounds how many
//! values may be *queued* in it; what bounds the payload actually alive at once is the serial shape of this
//! task - it is one `select!`, so it is in exactly one branch at a time, and it awaits an acknowledgment
//! before it builds the next piece. The distinction matters because a depth-one channel is not a one-buffer
//! bound: the consumer can hold a chunk it dequeued while a successor is already queued behind it, which is
//! exactly the peak [crate::tcp::flow_footprint] charges two chunks for in this direction. With that said,
//! each direction's channel is what applies the pressure:
//!
//! - client to upstream: when this task cannot keep up, the engine's channel fills, the engine stops draining
//!   the stack's receive buffer, the buffer fills, and the advertised window closes. The client is throttled
//!   by TCP itself.
//! - upstream to client: when the engine cannot keep up, this task's channel fills, it stops reading the
//!   upstream socket, and the *upstream's* window closes. The remote is throttled the same way.
//!
//! Neither direction drops data to relieve pressure, which is what separates a terminated stream from the
//! relayed datagrams next door: a datagram nobody promised to deliver may be dropped, a byte in an
//! acknowledged stream may not.
//!
//! Every one of those waits therefore also races the flow's token, and that is not defensive. Each of them can
//! block for as long as a peer chooses: a write into a full send buffer waits on a remote that stopped
//! reading, a read waits on one that says nothing, and an event send waits on an engine that has stopped
//! draining in order to retire this very flow. A retirement has to be abortive, so none of them may be what a
//! retirement waits for - and this task *finishing* is what the engine joins. Finishing is not the same as
//! the flow being removed: a clean completion whose client is still closing detaches the flow, which keeps
//! its socket and its charge until that teardown ends. See [crate::tcp::terminal].

use std::io;
use std::time::Duration;

use crate::workers::Ended;
use smoltcp::iface::SocketHandle;
use socket2::SockRef;
use tokio::io::AsyncReadExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::preempt::{shutdown, write_all, Written};

use crate::owned::Owned;
use crate::report;

/// One read from the upstream socket. Sized to what one segment toward the client can carry, so a full read
/// turns into one segment rather than being re-split by the stack.
pub(crate) const READ_CHUNK: usize = 1500;

/// The per-flow mailbox this half produces into, and the identity every one of its signals carries.
///
/// Both live in [vpnhotspotd::shared::mailbox] rather than here, because what they carry is an ownership rule
/// - one chunk alive at a time - and that is checkable on a host where a smoltcp socket is not.
pub(crate) use crate::mailbox::{Chunk, Handed, Mailbox as Post, Payload};

/// This flow's mailbox, at the handle type the client-side stack names its slots by.
pub(crate) type Mailbox = Post<SocketHandle>;

/// A payload-free wake naming exactly which flow may have work.
pub(crate) type Event = crate::mailbox::Marker<SocketHandle>;

/// `sweep` is the engine's own token rather than this flow's: it is cancelled only when the whole table is
/// being retired, which is the one case where this socket must be torn down abortively rather than closed.
pub(crate) async fn splice(
    stream: TcpStream,
    mut mailbox: Mailbox,
    mut downstream: mpsc::Receiver<Owned>,
    cancel: CancellationToken,
    sweep: CancellationToken,
) -> Ended {
    let (mut reader, mut writer) = stream.into_split();
    let mut failure = None;
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            outgoing = downstream.recv() => match outgoing {
                Some(bytes) => {
                    match write_all(&mut writer, &bytes, &cancel).await {
                        Written::Done => {}
                        Written::Cancelled => break,
                        Written::Failed(e) => {
                            failure = Some(("shizuku.tcp_upstream_write", e));
                            break;
                        }
                    }
                }
                // The engine dropped its sender, which is the client's half-close: shut down writing and keep
                // reading, because the remote may still have data to send.
                None => {
                    match shutdown(&mut writer, &cancel).await {
                        Written::Done => {}
                        Written::Cancelled => break,
                        Written::Failed(e) => {
                            failure = Some(("shizuku.tcp_upstream_shutdown", e));
                            break;
                        }
                    }
                    if let Err(e) = drain(&mut reader, &mut mailbox, &cancel, &mut buffer).await {
                        failure = Some(("shizuku.tcp_upstream_read", e));
                    }
                    break;
                }
            },
            read = reader.read(&mut buffer) => match read {
                Ok(0) => {
                    // Orderly close from the remote; the client is told the same way rather than reset. Waited
                    // on like any payload, because a clean end may not overtake the bytes before it and this
                    // task may not return while the client's stack has not taken them.
                    if !mailbox.hand_over(Chunk::Finished, &cancel).await {
                        break;
                    }
                    // Nothing left to read, but the client may still be sending, so this keeps servicing the
                    // downstream direction until the engine closes the flow.
                    if let Err(e) = forward_only(&mut writer, &mut downstream, &cancel).await {
                        failure = Some(("shizuku.tcp_upstream_write", e));
                    }
                    break;
                }
                Ok(bytes) => {
                    // Awaited rather than tried, and awaited until *consumed* rather than until delivered:
                    // that is the upstream-to-client bound. This loop stops until the client's stack has
                    // taken the whole chunk, so the upstream's own window closes instead of bytes being
                    // dropped or a second chunk being read behind the first.
                    if !mailbox.hand_over(Chunk::Payload(Payload::new(buffer[..bytes].to_vec())), &cancel).await {
                        break;
                    }
                }
                Err(e) => {
                    failure = Some(("shizuku.tcp_upstream_read", e));
                    break;
                }
            },
        }
    }
    // Abortive only on a sweep, and that distinction is the whole point: an ordinary close is a stream that
    // ended, while a swept one must not keep transmitting queued bytes, retransmissions and the FIN over the
    // `Network` the session is leaving. Set before the halves are dropped, because dropping them is the close.
    if sweep.is_cancelled() {
        if let Err(e) = reader
            .reunite(writer)
            .map_err(io::Error::other)
            .and_then(|stream| SockRef::from(&stream).set_linger(Some(Duration::ZERO)))
        {
            // Reported and closed anyway: the residue is a drained send queue on a network the session is
            // leaving, which is not worth ending a working session over.
            report::io("shizuku.tcp_sweep", e);
        }
    } else {
        // Explicit, because dropping these is the close: the engine joins this task before it may refund, so
        // the descriptor has to be gone by the time this returns rather than merely unreferenced. The refund
        // itself may come later still - a clean completion detaches the flow rather than ending it.
        drop(reader);
        drop(writer);
    }
    match failure {
        Some((context, error)) if !expected(&error) => Ended::Failed { context, error },
        // A peer that resets, times out or vanishes is the network being the network, and every flow's client
        // learns of it the one way a terminated flow can say it: a reset. One line names it, and the engine
        // prints that rather than raising a report per hostile peer.
        Some((_, error)) => Ended::Reported(error.to_string()),
        None => Ended::Expected,
    }
}

/// Whether a failure is the peer ending the exchange rather than the daemon failing at it. Only these are
/// classified as expected: anything else is the daemon's own I/O going wrong and is raised as a report.
fn expected(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
            | io::ErrorKind::TimedOut
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::NetworkDown
    )
}

/// Keeps reading the upstream after the client has half-closed, so a request-then-response protocol still gets
/// its response.
async fn drain(
    reader: &mut OwnedReadHalf,
    mailbox: &mut Mailbox,
    cancel: &CancellationToken,
    buffer: &mut [u8],
) -> io::Result<()> {
    loop {
        let read = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            read = reader.read(buffer) => read?,
        };
        if read == 0 {
            mailbox.hand_over(Chunk::Finished, cancel).await;
            return Ok(());
        }
        if !mailbox
            .hand_over(
                Chunk::Payload(Payload::new(buffer[..read].to_vec())),
                cancel,
            )
            .await
        {
            return Ok(());
        }
    }
}

/// Keeps writing to the upstream after the remote has finished sending, for the same reason in reverse.
async fn forward_only(
    writer: &mut OwnedWriteHalf,
    downstream: &mut mpsc::Receiver<Owned>,
    cancel: &CancellationToken,
) -> io::Result<()> {
    loop {
        let outgoing = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            outgoing = downstream.recv() => outgoing,
        };
        match outgoing {
            Some(bytes) => match write_all(writer, &bytes, cancel).await {
                Written::Done => {}
                Written::Cancelled => return Ok(()),
                Written::Failed(e) => return Err(e),
            },
            // The client half-closed too, so both directions are done.
            None => {
                return match shutdown(writer, cancel).await {
                    Written::Done | Written::Cancelled => Ok(()),
                    Written::Failed(e) => Err(e),
                }
            }
        }
    }
}

/// Hands one buffer over in `quantum`-sized pieces, one at a time.
///
/// The loop is the bound. Each piece is copied out of `whole` immediately before it is handed over, and the
/// next is not copied until the owner has acknowledged consuming it - so exactly one piece allocation exists
/// at any moment, whatever the size of the buffer being sent. Building the pieces first and handing them over
/// afterwards is the shape this replaces: it satisfies the mailbox's depth and holds a second copy of the
/// whole response while it does, which is precisely the allocation the depth was supposed to bound.
///
/// `whole` itself stays alive throughout, because it is what the pieces are copied from - and that is what
/// the delivery grant covers: the answer, this framed copy, and one piece.
pub(crate) async fn hand_over_in_pieces(
    mailbox: &mut Mailbox,
    whole: &[u8],
    quantum: usize,
    cancel: &CancellationToken,
) -> Handed {
    for piece in whole.chunks(quantum) {
        // Allocated here, counted here, handed over, and gone before the next exists. The count is held
        // across the handover rather than released at the send, because the producer does not build the next
        // piece until this one has been acknowledged - which is what depth one actually says.
        // Counted by the payload itself, so it stays counted wherever it ends up: consumed by the engine,
        // or left in the mailbox by a cancellation.
        if !mailbox
            .hand_over(Chunk::Payload(Payload::new(piece.to_vec())), cancel)
            .await
        {
            return Handed::Cancelled;
        }
    }
    Handed::Complete
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::Marker;
    use crate::owned;

    /// Everything the engine side of a mailbox does, so a test can drive the real handover.
    struct Consumer {
        chunks: mpsc::Receiver<Chunk>,
        ready: mpsc::Receiver<Event>,
        consumed: mpsc::Sender<()>,
    }

    fn pair(identity: Event) -> (Mailbox, Consumer) {
        let (chunk_tx, chunks) = mpsc::channel(1);
        let (ready_tx, ready) = mpsc::channel(4);
        let (consumed, ack) = mpsc::channel(1);
        (
            Mailbox {
                chunks: chunk_tx,
                ready: ready_tx,
                consumed: ack,
                identity,
            },
            Consumer {
                chunks,
                ready,
                consumed,
            },
        )
    }

    fn handle() -> SocketHandle {
        let mut sockets = smoltcp::iface::SocketSet::new(Vec::with_capacity(1));
        sockets.add(smoltcp::socket::tcp::Socket::new(
            smoltcp::socket::tcp::SocketBuffer::new(vec![0u8; 64]),
            smoltcp::socket::tcp::SocketBuffer::new(vec![0u8; 64]),
        ))
    }

    /// Exactly one piece exists at a time, however large the buffer being handed over - counted where the
    /// pieces are allocated rather than where they are received.
    ///
    /// The failure this closes is a producer that builds every piece before the first handover: the mailbox's
    /// depth is still one, and a whole second copy of the response is alive in as many allocations as the
    /// quantum divides into it. Invisible to the consumer, which sees one at a time either way.
    #[tokio::test]
    async fn only_one_piece_is_alive_at_a_time() {
        owned::reset();
        let identity = Marker {
            handle: handle(),
            worker: 7,
        };
        let (mut mailbox, mut consumer) = pair(identity);
        let cancel = CancellationToken::new();
        // A maximum DNS response, framed: two bytes of prefix and 65535 of message.
        let whole = vec![0x5au8; 65_535 + 2];
        let quantum = 1_500;

        let producer = tokio::spawn(async move {
            let handed = hand_over_in_pieces(&mut mailbox, &whole, quantum, &cancel).await;
            (handed, whole.len())
        });

        let mut pieces = 0usize;
        let mut bytes = 0usize;
        while let Some(chunk) = consumer.chunks.recv().await {
            let Chunk::Payload(piece) = chunk else {
                panic!("only payload is handed over here")
            };
            pieces += 1;
            bytes += piece.len();
            assert!(piece.len() <= quantum);
            assert_eq!(
                consumer.ready.recv().await,
                Some(identity),
                "every piece is announced by its own identity"
            );
            let (live, _) = owned::peak();
            assert_eq!(live.buffers, 1, "piece {pieces}: one, and only one");
            drop(piece);
            // The acknowledgment is what lets the next one be built.
            if consumer.consumed.send(()).await.is_err() {
                break;
            }
        }
        let (handed, whole) = producer.await.expect("the producer finished");
        assert_eq!(handed, Handed::Complete);
        assert_eq!(bytes, whole, "every byte went, once");
        assert!(pieces > 40, "{pieces} pieces");
        let (live, peak) = owned::peak();
        assert_eq!(live.buffers, 0, "and none is left");
        assert_eq!(peak.buffers, 1, "only one piece may exist at a time");
        assert!(peak.bytes <= quantum, "{} bytes at the peak", peak.bytes);
    }

    /// A cancellation part-way stops the handover where it is, and nothing further is built.
    #[tokio::test]
    async fn cancellation_stops_the_handover_where_it_is() {
        owned::reset();
        let identity = Marker {
            handle: handle(),
            worker: 9,
        };
        let (mut mailbox, mut consumer) = pair(identity);
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let whole = vec![0u8; 10_000];

        let producer =
            tokio::spawn(
                async move { hand_over_in_pieces(&mut mailbox, &whole, 1_000, &token).await },
            );

        // Two pieces through, then cancelled while the third waits for its acknowledgment.
        let mut taken = 0usize;
        for _ in 0..2 {
            assert!(consumer.chunks.recv().await.is_some());
            assert_eq!(consumer.ready.recv().await, Some(identity));
            taken += 1;
            consumer
                .consumed
                .send(())
                .await
                .expect("the producer waits");
        }
        cancel.cancel();
        assert_eq!(producer.await.expect("finished"), Handed::Cancelled);
        assert_eq!(
            taken, 2,
            "and nothing past the cancellation was acknowledged"
        );
        let (live, _) = owned::peak();
        assert_eq!(
            live.buffers, 0,
            "the piece in flight went with the producer"
        );
    }
}
