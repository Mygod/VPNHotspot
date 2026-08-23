//! The waits a worker may be preempted out of.
//!
//! Every wait a dataplane worker performs is on something a peer or a saturated owner controls: a write into a
//! send buffer the remote has stopped draining, a half-close whose FIN does not fit that buffer, an event
//! handed to an owner that has stopped reading in order to retire this very worker. None of them is bounded by
//! anything the daemon decides.
//!
//! Retirement, on the other hand, is bounded by design: the app is waiting on the acknowledgement that follows
//! it, and the acknowledgement waits for these workers to finish. So each of these waits races the worker's own
//! token, and a retired worker abandons the operation rather than the operation delaying the retirement. That
//! is safe precisely because a retired flow's socket is then closed abortively - there is no half-written
//! stream left for anyone to read - and because a payload a retired owner would have refused is a payload
//! nobody was promised.

use std::io;

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How far one write got.
pub enum Written {
    Done,
    /// The worker is being retired, so the rest of the operation is abandoned. Whatever had not reached the
    /// remote is lost with the connection, which is what an abortive close means.
    Cancelled,
    Failed(io::Error),
}

/// Writes one whole chunk, or gives up on it when the worker is retired.
pub async fn write_all<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    cancel: &CancellationToken,
) -> Written {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Written::Cancelled,
        written = writer.write_all(bytes) => match written {
            Ok(()) => Written::Done,
            Err(e) => Written::Failed(e),
        },
    }
}

/// Half-closes the write direction, or gives up when the worker is retired. `shutdown` waits for the FIN to be
/// queued, which a full send buffer defers.
pub async fn shutdown<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cancel: &CancellationToken,
) -> Written {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Written::Cancelled,
        shut = writer.shutdown() => match shut {
            Ok(()) => Written::Done,
            Err(e) => Written::Failed(e),
        },
    }
}

/// Hands one event to the owner, waiting for room but not past a cancellation. `false` means the worker is
/// stopping instead: either it is being retired, or the owner is gone and there is nobody to deliver to.
///
/// Waiting for room is the backpressure the transports depend on - it is what closes a window or lets a
/// kernel receive buffer absorb a burst - so this waits rather than trying. What it must not do is wait on an
/// owner that has stopped draining because it is retiring this worker.
pub async fn hand_over<T>(events: &mpsc::Sender<T>, event: T, cancel: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        sent = events.send(event) => sent.is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::duplex;

    use super::*;

    #[tokio::test]
    async fn a_write_the_peer_will_not_take_is_abandoned_when_retirement_arrives() {
        // One byte of buffer and a peer that never reads, which is a stalled remote with no timer on it.
        let (mut writer, _peer) = duplex(1);
        let cancel = CancellationToken::new();
        let stalled = write_all(&mut writer, &[0u8; 64], &cancel);
        let retiring = async {
            tokio::task::yield_now().await;
            cancel.cancel();
        };
        let (written, ()) = tokio::join!(stalled, retiring);
        assert!(matches!(written, Written::Cancelled));
    }

    #[tokio::test]
    async fn a_half_close_that_cannot_be_queued_is_abandoned_too() {
        let (mut writer, peer) = duplex(1);
        let cancel = CancellationToken::new();
        // Filled first, so the FIN has nowhere to go: `shutdown` flushes before it closes.
        assert!(matches!(
            write_all(&mut writer, &[0u8], &cancel).await,
            Written::Done
        ));
        drop(peer);
        cancel.cancel();
        assert!(matches!(
            shutdown(&mut writer, &cancel).await,
            Written::Cancelled
        ));
    }

    #[tokio::test]
    async fn an_event_a_full_owner_cannot_take_does_not_hold_up_retirement() {
        let (events, _events) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        // The owner's queue is full and it has stopped reading, which is exactly its state while it retires.
        assert!(hand_over(&events, 1u8, &cancel).await);
        cancel.cancel();
        assert!(!hand_over(&events, 2u8, &cancel).await);
    }

    #[tokio::test]
    async fn an_owner_that_is_gone_ends_the_worker_without_a_cancellation() {
        let (events, receiver) = mpsc::channel(1);
        drop(receiver);
        assert!(!hand_over(&events, 3u8, &CancellationToken::new()).await);
    }

    #[tokio::test]
    async fn an_uncancelled_write_and_half_close_still_complete() {
        let (mut writer, mut peer) = duplex(64);
        let cancel = CancellationToken::new();
        assert!(matches!(
            write_all(&mut writer, b"query", &cancel).await,
            Written::Done
        ));
        assert!(matches!(
            shutdown(&mut writer, &cancel).await,
            Written::Done
        ));
        let mut read = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut read)
            .await
            .expect("the peer reads what was written");
        assert_eq!(read, b"query");
    }
}
