//! One terminated TCP flow's upstream half: the selected-network socket, and the splice between it and the
//! client-side stack.
//!
//! Backpressure is the point of this shape, and it is bounded in both directions without a byte counter
//! anywhere. What bounds each direction is a *permit* rather than a message: a producer waits for room in the
//! queue it is producing into, and the consumer taking a value is what frees it. Neither direction needs a
//! round trip to move a byte, and neither may drop one:
//!
//! - client to upstream: when this task cannot keep up, the engine's queue fills, the engine stops draining
//!   the stack's receive buffer, the buffer fills, and the advertised window closes. The client is throttled
//!   by TCP itself. The engine learns that room is back from the queue itself - see
//!   [vpnhotspotd::shared::room] - so a full queue does not leave the window closed until something unrelated
//!   happens to wake the engine.
//! - upstream to client: when the engine cannot keep up, this task's queue fills, it stops reading the
//!   upstream socket, and the *upstream's* window closes. The remote is throttled the same way. Up to
//!   [vpnhotspotd::shared::flow_budget::READ_AHEAD] chunks may be queued ahead of the engine, which is what
//!   lets this half read while the engine is writing the chunk before it rather than waiting to be told each
//!   one was consumed.
//!
//! Neither direction drops data to relieve pressure, which is what separates a terminated stream from the
//! relayed datagrams next door: a datagram nobody promised to deliver may be dropped, a byte in an
//! acknowledged stream may not. **An abortive ending is the exception, and read-ahead widens it.** A flow
//! reset by a retirement, an idle expiry or an upstream that failed or vanished discards whatever the engine
//! has not written into the client's send buffer yet - which is now up to one send buffer's worth of queued
//! chunks rather than one. The client learns of it the one way a terminated flow can say it, a reset, and
//! nothing about the clean path changes: an orderly end of stream is queued behind the payload it follows and
//! delivered after it, and a clean completion detaches the flow so the engine goes on draining what this task
//! left queued.
//!
//! Every one of those waits therefore also races the flow's token, and that is not defensive. Each of them can
//! block for as long as a peer chooses: a write into a full send buffer waits on a remote that stopped
//! reading, a read waits on one that says nothing, and a handover waits on an engine that has stopped
//! draining in order to retire this very flow. A retirement has to be abortive, so none of them may be what a
//! retirement waits for - and this task *finishing* is what the engine joins. Finishing is not the same as
//! the flow being removed: a clean completion whose client is still closing detaches the flow, which keeps
//! its socket and its charge until that teardown ends. See `shizuku/tcp/terminal.rs`.

use std::io;
use std::time::Duration;

use crate::shizuku::workers::Ended;
use smoltcp::iface::SocketHandle;
use socket2::SockRef;
use tokio::io::AsyncReadExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::fair::FlowId;
use vpnhotspotd::shared::preempt::{shutdown, write_all, Written};

use vpnhotspotd::shared::flow_budget::READ_CHUNK;
use vpnhotspotd::shared::mailbox;

use crate::report;
use crate::shizuku::owned::Owned;

/// One piece of payload on its way to a client's stack, counted for exactly as long as it exists.
pub(crate) type Payload = Owned;

/// What this half produces into its flow's mailbox.
///
/// The mailbox itself lives in [vpnhotspotd::shared::mailbox] rather than here, because what it carries is
/// the stack-neutral ownership rule about how many chunks may be alive at once - and which of its two
/// handovers a producer uses is the difference between ordinary traffic and DNS-over-TCP.
pub(crate) type Chunk = mailbox::Chunk<Payload>;

/// This flow's mailbox, at the handle type the client-side stack names its slots by.
pub(crate) type Mailbox = mailbox::Mailbox<SocketHandle, Payload>;

pub(crate) use vpnhotspotd::shared::mailbox::Handed;

/// One flow, named on the requests its DNS-over-TCP transport makes of the ingress owner.
///
/// Not a wake: nothing travels between a flow's task and its owner to say payload is waiting, because each
/// flow's own queue is what wakes the owner - see [vpnhotspotd::shared::transfer].
pub(crate) type Event = FlowId<SocketHandle>;

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
                    // Orderly close from the remote; the client is told the same way rather than reset. Queued
                    // like any payload, because a clean end may not overtake the bytes before it - the queue is
                    // ordered and the engine takes one chunk at a time, so the end of stream reaches the client's
                    // stack only once everything queued in front of it has. This task may return with those
                    // chunks still queued: a clean terminal detaches the flow rather than removing it, and the
                    // engine goes on delivering them.
                    if !mailbox.hand_off(Chunk::Finished, &cancel).await {
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
                    // Awaited rather than tried, and awaited for *room* rather than for consumption: that is
                    // the upstream-to-client bound. Up to one send buffer's worth of chunks may be waiting,
                    // and this loop stops only once they are, so the upstream's own window closes instead of
                    // bytes being dropped - while the engine taking one chunk is what lets the next read
                    // happen, with no signal of ours in between.
                    if !mailbox.hand_off(Chunk::Payload(Payload::new(buffer[..bytes].to_vec())), &cancel).await {
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
            mailbox.hand_off(Chunk::Finished, cancel).await;
            return Ok(());
        }
        if !mailbox
            .hand_off(
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
/// The acknowledgment is the bound, and it is why this path may not use the queued handover ordinary traffic
/// does. Each piece is copied out of `whole` immediately before it is handed over, and the next is not copied
/// until the owner has acknowledged consuming it - so exactly one piece allocation exists at any moment,
/// whatever the size of the buffer being sent. Queueing them instead would satisfy the mailbox's depth and
/// hold as many copies of the response as the read-ahead is deep, which is precisely the allocation the
/// delivery grant exists to bound.
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
        // piece until this one has been acknowledged - which is what the acknowledged handover is for.
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
