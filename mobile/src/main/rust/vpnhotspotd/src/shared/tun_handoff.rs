//! Bounded handoff to the serial TUN writer.
//!
//! Remote-provoked output is admitted as complete logical datagrams so fragments cannot be partially queued
//! or interleaved. Guarded IPv4 datagrams return their actual write outcome to the Identification allocator.
use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::shared::ipv4_identification::{Guarded, Terminal};

/// Number of complete datagrams queued for the serial TUN writer.
///
/// **Resource:** one heap-owned datagram batch, containing packets no larger than the interface MTU.
///
/// **Derivation:** one is the minimum [tokio::sync::mpsc::channel] capacity and lets the owner prepare one
/// batch while the serial writer owns another, for at most two daemon-owned datagrams.
///
/// **Failure mode:** a stalled TUN could otherwise let Internet-provoked output grow daemon memory.
///
/// **Exhaustion:** non-TCP output drops and counts the complete batch; TCP stops polling and retains output
/// in smoltcp until capacity returns. See [crate::shared::tcp_device::quiesce].
pub const DATAGRAMS: usize = 1;

/// Number of guarded-datagram settlements queued for the owner.
///
/// **Resource:** one fixed-size [Terminal] record with no payload.
///
/// **Derivation:** one suffices because the writer settles before receiving another batch; at most one
/// additional settlement can be blocked in [Queue::settle].
///
/// **Failure mode:** losing a settlement leaves its Identification outstanding and prevents tuple reuse.
///
/// **Exhaustion:** [Queue::settle] backpressures the writer. The owner prioritizes settlements even while
/// waiting for writer capacity; cancellation or owner closure ends the wait.
pub const SETTLEMENTS: usize = 1;

/// One complete logical datagram: every packet it became, and the Identification they all carry.
#[derive(Debug)]
pub struct Batch {
    packets: Vec<Vec<u8>>,
    /// The guarded datagram these packets belong to, or `None` for everything carrying no Identification
    /// this daemon issued - which is all atomic IPv4 output, and everything IPv6.
    guarded: Option<Guarded>,
}

impl Batch {
    pub fn new(packets: Vec<Vec<u8>>, guarded: Option<Guarded>) -> Self {
        Self { packets, guarded }
    }

    pub fn packets(&self) -> &[Vec<u8>] {
        &self.packets
    }

    pub fn guarded(&self) -> Option<Guarded> {
        self.guarded
    }

    pub fn into_packets(self) -> Vec<Vec<u8>> {
        self.packets
    }
}

/// Last successful write within one logical datagram.
///
/// A partial write still exposes the Identification, so settlement uses the last successful fragment time.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    written: Option<Instant>,
}

impl Progress {
    /// Records a successful write time, or stops the batch on `None`.
    pub fn packet(&mut self, at: Option<Instant>) -> bool {
        match at {
            Some(at) => {
                self.written = Some(at);
                true
            }
            None => false,
        }
    }

    /// The last moment a packet of this datagram reached the wire, if any did.
    pub fn written(self) -> Option<Instant> {
        self.written
    }

    /// Builds the guarded settlement, if this datagram has one.
    pub fn terminal(self, guarded: Option<Guarded>) -> Option<Terminal> {
        guarded.map(|guarded| match self.written {
            Some(at) => Terminal::wrote(guarded, at),
            None => Terminal::unwritten(guarded),
        })
    }
}

/// Why a complete datagram was not taken. Both endings drop the whole batch; neither leaves a prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// The one slot still holds a datagram the writer has not taken.
    Full,
    /// The writer task has dropped its receiving end, which already ends this session through its own result.
    Closed,
}

/// The sole datagram producer; this makes [Writer::accepting] exact.
pub struct Writer {
    datagrams: mpsc::Sender<Batch>,
}

impl Writer {
    /// Hands off one complete datagram without blocking the owner.
    pub fn enqueue(&self, batch: Batch) -> Result<(), Rejected> {
        self.datagrams.try_send(batch).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => Rejected::Full,
            mpsc::error::TrySendError::Closed(_) => Rejected::Closed,
        })
    }

    /// Whether enqueue can complete immediately. Closure counts as accepting because refusal is immediate.
    pub fn accepting(&self) -> bool {
        self.datagrams.capacity() > 0 || self.datagrams.is_closed()
    }

    /// Waits for capacity without retaining the permit; used only while the handoff is open and full.
    pub async fn accepted(&self) {
        match self.datagrams.reserve().await {
            // The owner is about to fill this slot.
            Ok(permit) => drop(permit),
            Err(_) => std::future::pending().await,
        }
    }
}

/// The receiving ends the writer task owns, kept together so a caller cannot wire up one without the other.
pub struct Queue {
    datagrams: mpsc::Receiver<Batch>,
    settlements: mpsc::Sender<Terminal>,
}

impl Queue {
    /// The next complete datagram to write. `None` when the owner is gone or this session was cancelled,
    /// both of which end the writer.
    pub async fn next(&mut self, cancel: &CancellationToken) -> Option<Batch> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            queued = self.datagrams.recv() => queued,
        }
    }

    /// Sends one guarded settlement before the writer receives another datagram.
    pub async fn settle(&mut self, terminal: Terminal, cancel: &CancellationToken) -> bool {
        tokio::select! {
            biased;
            () = cancel.cancelled() => false,
            sent = self.settlements.send(terminal) => sent.is_ok(),
        }
    }
}

/// The two channels one session's TUN writer owns, and the settlement receiver the dataplane owner keeps.
pub fn channel() -> (Writer, Queue, mpsc::Receiver<Terminal>) {
    let (datagrams, queued) = mpsc::channel(DATAGRAMS);
    let (settlements, settled) = mpsc::channel(SETTLEMENTS);
    (
        Writer { datagrams },
        Queue {
            datagrams: queued,
            settlements,
        },
        settled,
    )
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use super::*;
    use crate::shared::ipv4_identification::{Ipv4Identifications, MDL};

    const CLIENT: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
    const REMOTE: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 1);

    fn fragments(count: usize) -> Vec<Vec<u8>> {
        (0..count).map(|index| vec![index as u8; 40]).collect()
    }

    fn guarded() -> Guarded {
        let opened = Instant::now();
        let mut identifications = Ipv4Identifications::new(opened);
        identifications
            .next((REMOTE, CLIENT, 17), opened + MDL)
            .expect("the quarantine has passed")
    }

    #[tokio::test]
    async fn a_full_handoff_rejects_the_next_complete_datagram_whole() {
        let (writer, mut queue, _settled) = channel();
        assert!(writer.accepting());
        assert_eq!(writer.enqueue(Batch::new(fragments(3), None)), Ok(()));
        assert!(!writer.accepting(), "the one slot is taken");

        let refused = Batch::new(fragments(4), None);
        assert_eq!(writer.enqueue(refused), Err(Rejected::Full));

        let cancel = CancellationToken::new();
        let taken = queue.next(&cancel).await.expect("the queued datagram");
        assert_eq!(
            taken.packets().len(),
            3,
            "the first datagram crossed whole, and the refused one left nothing behind it"
        );
        assert!(writer.accepting());
        assert_eq!(writer.enqueue(Batch::new(fragments(5), None)), Ok(()));
        assert_eq!(
            queue
                .next(&cancel)
                .await
                .expect("the second datagram")
                .packets()
                .len(),
            5,
            "a fragmented datagram is admitted with every fragment or with none"
        );
    }

    #[tokio::test]
    async fn a_refused_datagram_never_reaches_the_writer_in_part() {
        let (writer, mut queue, _settled) = channel();
        writer
            .enqueue(Batch::new(fragments(2), None))
            .expect("the empty slot");
        for count in [1usize, 7, 64] {
            assert_eq!(
                writer.enqueue(Batch::new(fragments(count), None)),
                Err(Rejected::Full),
                "a {count}-fragment datagram is refused as one"
            );
        }
        let cancel = CancellationToken::new();
        assert_eq!(
            queue
                .next(&cancel)
                .await
                .expect("the one taken")
                .packets()
                .len(),
            2
        );
        assert!(
            queue.datagrams.try_recv().is_err(),
            "nothing of the refused datagrams was queued"
        );
    }

    #[tokio::test]
    async fn the_owner_wait_is_released_by_the_writer_taking_the_batch() {
        let (writer, mut queue, _settled) = channel();
        writer
            .enqueue(Batch::new(fragments(2), None))
            .expect("the empty slot");
        let cancel = CancellationToken::new();
        let waiting = tokio::spawn(async move {
            // Cannot be observed as accepting until the writer takes what is queued.
            writer.accepted().await;
            assert!(writer.accepting());
            writer
        });
        let taken = queue.next(&cancel).await.expect("the queued datagram");
        assert_eq!(taken.packets().len(), 2);
        waiting.await.expect("the owner wakes when the slot frees");
    }

    #[tokio::test]
    async fn a_closed_handoff_refuses_rather_than_stalling_its_producer() {
        let (writer, queue, _settled) = channel();
        drop(queue);
        assert_eq!(
            writer.enqueue(Batch::new(fragments(1), None)),
            Err(Rejected::Closed)
        );
        assert!(
            writer.accepting(),
            "a closed handoff must not look like one that will drain"
        );
    }

    #[tokio::test]
    async fn one_guarded_ending_crosses_at_a_time_and_closure_ends_the_writer() {
        let (writer, mut queue, mut settled) = channel();
        let cancel = CancellationToken::new();
        let guarded = guarded();
        writer
            .enqueue(Batch::new(fragments(3), Some(guarded)))
            .expect("the empty slot");
        let taken = queue.next(&cancel).await.expect("the queued datagram");
        assert_eq!(taken.guarded(), Some(guarded));
        let at = Instant::now();
        assert!(queue.settle(Terminal::wrote(guarded, at), &cancel).await);
        assert_eq!(settled.recv().await.map(|t| t.guarded()), Some(guarded));

        drop(settled);
        assert!(
            !queue.settle(Terminal::unwritten(guarded), &cancel).await,
            "an owner that is gone ends the writer rather than losing endings quietly"
        );
    }

    #[test]
    fn a_batch_no_packet_of_which_was_written_owes_an_unwritten_ending() {
        let guarded = guarded();
        // A batch the writer refused before its first write: packetization was invalid, so no packet outcome
        // was ever recorded.
        let refused = Progress::default();
        assert_eq!(refused.written(), None);
        assert_eq!(
            refused.terminal(Some(guarded)),
            Some(Terminal::unwritten(guarded))
        );

        // And one cancelled while its first packet waited for the descriptor to become writable.
        let mut cancelled = Progress::default();
        assert!(
            !cancelled.packet(None),
            "a packet that never went stops the batch"
        );
        assert_eq!(cancelled.written(), None);
        assert_eq!(
            cancelled.terminal(Some(guarded)),
            Some(Terminal::unwritten(guarded))
        );
    }

    #[test]
    fn a_partially_written_batch_owes_its_last_successful_write() {
        let guarded = guarded();
        let base = Instant::now();
        let first = base + Duration::from_millis(1);
        let second = base + Duration::from_millis(2);

        // Two fragments reached the wire and the third was cancelled: the ending is dated to the second.
        let mut cancelled = Progress::default();
        assert!(cancelled.packet(Some(first)));
        assert!(cancelled.packet(Some(second)));
        assert!(!cancelled.packet(None));
        assert_eq!(cancelled.written(), Some(second));
        assert_eq!(
            cancelled.terminal(Some(guarded)),
            Some(Terminal::wrote(guarded, second))
        );

        // Same for a batch whose next write failed fatally, which records no outcome for that packet at all.
        let mut failed = Progress::default();
        assert!(failed.packet(Some(first)));
        assert_eq!(
            failed.terminal(Some(guarded)),
            Some(Terminal::wrote(guarded, first))
        );

        // A batch every packet of which went is the same decision, dated to the last of them.
        let mut whole = Progress::default();
        assert!(whole.packet(Some(first)));
        assert!(whole.packet(Some(second)));
        assert_eq!(
            whole.terminal(Some(guarded)),
            Some(Terminal::wrote(guarded, second))
        );
    }

    #[test]
    fn an_unguarded_batch_owes_no_ending_however_far_it_got() {
        let mut progress = Progress::default();
        assert_eq!(progress.terminal(None), None);
        assert!(progress.packet(Some(Instant::now())));
        assert_eq!(progress.terminal(None), None);
    }

    /// Polls one future exactly once, which is how a writer parked on the settlement slot is observed
    /// without letting it make progress it should not be able to make.
    async fn polled<F: std::future::Future>(
        held: std::pin::Pin<&mut F>,
    ) -> std::task::Poll<F::Output> {
        let mut held = Some(held);
        std::future::poll_fn(move |cx| {
            std::task::Poll::Ready(held.take().expect("polled once").poll(cx))
        })
        .await
    }

    #[tokio::test]
    async fn a_second_ending_waits_for_the_owner_rather_than_being_dropped() {
        let (_writer, mut queue, mut settled) = channel();
        let cancel = CancellationToken::new();
        let guarded = guarded();
        let at = Instant::now();

        // The one slot takes the first ending without waiting.
        assert!(queue.settle(Terminal::wrote(guarded, at), &cancel).await);

        // The second waits. Together they are the whole of what the settlement path can own: one queued and
        // one the writer is blocked handing over.
        {
            let mut second = std::pin::pin!(queue.settle(Terminal::unwritten(guarded), &cancel));
            assert!(
                polled(second.as_mut()).await.is_pending(),
                "a full settlement slot backs the writer up rather than losing an ending"
            );
            // Taking the queued ending is what releases it - which is why the owner keeps that arm ahead of
            // everything else it selects on.
            assert_eq!(
                settled.recv().await.map(|terminal| terminal.written()),
                Some(Some(at))
            );
            assert!(second.await);
        }
        assert_eq!(
            settled.recv().await.map(|terminal| terminal.written()),
            Some(None),
            "and the second ending is the one the writer was holding, not a replacement"
        );
    }

    #[tokio::test]
    async fn cancellation_releases_a_writer_blocked_on_the_settlement_slot() {
        let (_writer, mut queue, _settled) = channel();
        let cancel = CancellationToken::new();
        let guarded = guarded();
        assert!(queue.settle(Terminal::unwritten(guarded), &cancel).await);
        let mut blocked = std::pin::pin!(queue.settle(Terminal::unwritten(guarded), &cancel));
        assert!(polled(blocked.as_mut()).await.is_pending());
        cancel.cancel();
        assert!(
            !blocked.await,
            "a cancelled session ends the writer instead of leaving it parked"
        );
    }

    #[tokio::test]
    async fn cancellation_releases_both_ends_of_the_handoff() {
        let (writer, mut queue, settled) = channel();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(queue.next(&cancel).await.is_none());
        assert!(!queue.settle(Terminal::unwritten(guarded()), &cancel).await);
        drop(writer);
        drop(settled);
    }
}
