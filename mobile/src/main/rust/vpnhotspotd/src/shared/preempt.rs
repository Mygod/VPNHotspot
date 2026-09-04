//! Cancellation-aware waits keep retirement independent of peer-controlled I/O.
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

/// Hands one event to the owner without imposing a shared queue capacity. Cancellation wins if it is already
/// observable when this turn is polled; `false` otherwise means the owner is gone and there is nobody to
/// deliver to. The owning worker remains sequential, so it cannot publish a second event until it resumes.
pub async fn hand_over<T>(
    events: &mpsc::UnboundedSender<T>,
    event: T,
    cancel: &CancellationToken,
) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        sent = async { events.send(event) } => sent.is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::duplex;

    use super::*;

    #[tokio::test]
    async fn a_write_the_peer_will_not_take_is_abandoned_when_retirement_arrives() {
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
    async fn owner_handoffs_do_not_wait_for_an_arbitrary_queue_slot() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        assert!(hand_over(&events, 1u8, &cancel).await);
        assert!(hand_over(&events, 2u8, &cancel).await);
        assert_eq!(receiver.recv().await, Some(1));
        assert_eq!(receiver.recv().await, Some(2));
    }

    #[tokio::test]
    async fn a_cancelled_handoff_does_not_publish() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(!hand_over(&events, 3u8, &cancel).await);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn an_owner_that_is_gone_ends_the_worker_without_a_cancellation() {
        let (events, receiver) = mpsc::unbounded_channel();
        drop(receiver);
        assert!(!hand_over(&events, 4u8, &CancellationToken::new()).await);
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
