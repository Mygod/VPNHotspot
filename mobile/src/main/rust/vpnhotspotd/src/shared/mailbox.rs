//! One flow's payload queue toward its client's stack, from the producer's side.
//!
//! `H` is whatever the owner names a flow's transport slot by - smoltcp's `SocketHandle` in the daemon - and
//! `P` is whatever one chunk of payload is carried as, so the ownership rules here do not depend on the stack
//! they serve. The owner's side of the same queue is [crate::shared::transfer].
//!
//! # Two disciplines, because two producers need different bounds
//!
//! **Queued read-ahead**, which ordinary relayed traffic uses. [Mailbox::hand_off] waits for *room* in the
//! flow's own queue and for nothing else, so an upstream half may read the next chunk while the owner is
//! still writing the previous one into the client's send buffer. What bounds it is that queue's depth, which
//! the flow was charged for before it existed - see [crate::shared::flow_budget] - and what frees the
//! producer is the owner taking a chunk out. No message travels either way: the queue is both the bound and
//! the wake.
//!
//! **Acknowledged handover**, which DNS-over-TCP keeps. [Mailbox::hand_over] waits for the *consumption*
//! acknowledgment, so exactly one chunk allocation is alive for that flow at any moment. That is not a slower
//! spelling of the same thing: a resolver answer is framed once and handed over in pieces copied out of that
//! framing, and the grant covering the exchange pays for the answer, the framed copy and *one* piece. A
//! producer that queued them instead would satisfy the queue's depth and violate the bound the grant
//! expresses.
//!
//! Both race the flow's cancellation token, because both wait on something a peer or a retiring owner
//! controls, and a retirement may not wait on either.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::shared::fair::FlowId;
use crate::shared::preempt::hand_over;

/// What one flow's producer puts in its queue.
pub enum Chunk<P> {
    /// Bytes for the client-side stack, in order.
    Payload(P),
    /// The producer finished sending. The client-side half-close follows, not a reset: a stream that ended
    /// cleanly upstream must end cleanly downstream too. Ordered strictly after whatever payload preceded it.
    Finished,
}

/// One flow's producing end: where its chunks go, and - for the one kind that waits - how it learns one was
/// consumed.
pub struct Mailbox<H, P> {
    pub chunks: mpsc::Sender<Chunk<P>>,
    /// Signalled by the owner when a chunk has been *consumed* - fully written into the client's send buffer,
    /// not merely delivered. Waiting for it is what makes [Mailbox::hand_over] mean one chunk alive; a
    /// producer using [Mailbox::hand_off] never reads it, because the queue's own permit is what bounds it.
    pub consumed: mpsc::Receiver<()>,
    /// Which flow this producer is, for the requests it makes of its owner. Not a wake and not part of any
    /// handover: the owner reaches this flow's queue through the flow it already holds.
    pub identity: FlowId<H>,
}

impl<H: Copy, P> Mailbox<H, P> {
    /// Queues one chunk, waiting for room and nothing else.
    ///
    /// `false` means the producer is stopping: it was cancelled, or the owner is gone. Waiting for room is
    /// the backpressure - a full queue stops the upstream half reading, which closes the *remote's* window
    /// rather than dropping a byte of an acknowledged stream - and racing the token is what keeps a
    /// retirement from waiting on an owner that has stopped draining in order to retire this very flow.
    pub async fn hand_off(&mut self, chunk: Chunk<P>, cancel: &CancellationToken) -> bool {
        hand_over(&self.chunks, chunk, cancel).await
    }

    /// Hands one chunk over and waits for the owner to consume it, or for the flow to be cancelled.
    ///
    /// `false` means cancelled: the owner has discarded this identity's pending payload and the wait is over
    /// whether or not anything was consumed.
    pub async fn hand_over(&mut self, chunk: Chunk<P>, cancel: &CancellationToken) -> bool {
        // The queue is empty by construction for this producer - it built nothing since its last
        // acknowledgment - so this send does not block. Racing the token anyway costs nothing and keeps the
        // invariant from being load-bearing.
        if !self.hand_off(chunk, cancel).await {
            return false;
        }
        tokio::select! {
            biased;
            () = cancel.cancelled() => false,
            acknowledged = self.consumed.recv() => acknowledged.is_some(),
        }
    }
}

/// How a sequential handover ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Handed {
    /// Every piece was handed over and acknowledged.
    Complete,
    /// Cancelled part-way. Whatever was in flight is the owner's to discard.
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::shared::flow_budget::SIZING;

    fn identity() -> FlowId<u32> {
        FlowId::new(7, 100)
    }

    /// One flow's producing end at the depth production builds it, with the owner's ends beside it.
    ///
    /// The owner's acknowledgment end has to stay alive for the length of a test even when nothing reads it,
    /// because a producer answers `false` for an owner that is *gone* exactly as it does for a cancellation.
    struct Wired {
        mailbox: Mailbox<u32, Vec<u8>>,
        incoming: mpsc::Receiver<Chunk<Vec<u8>>>,
        consumed: mpsc::Sender<()>,
    }

    fn wired(depth: usize) -> Wired {
        let (chunks, incoming) = mpsc::channel(depth);
        let (consumed, acknowledged) = mpsc::channel(SIZING.control);
        Wired {
            mailbox: Mailbox {
                chunks,
                consumed: acknowledged,
                identity: identity(),
            },
            incoming,
            consumed,
        }
    }

    #[tokio::test]
    async fn the_producer_fills_its_whole_charged_read_ahead_with_nobody_consuming() {
        // Production's own depth, and production's own shape: one queue per flow and no second path a
        // producer could block on.
        let Wired {
            mut mailbox,
            mut incoming,
            consumed: _consumed,
        } = wired(SIZING.read_ahead);
        let cancel = CancellationToken::new();
        for byte in 0..SIZING.read_ahead {
            assert!(
                mailbox
                    .hand_off(Chunk::Payload(vec![byte as u8]), &cancel)
                    .await,
                "chunk {byte} of the charged read-ahead"
            );
        }
        // Exactly the depth that was charged, in order, and nothing beyond it.
        assert_eq!(incoming.len(), SIZING.read_ahead);
        for byte in 0..SIZING.read_ahead {
            match incoming.try_recv() {
                Ok(Chunk::Payload(bytes)) => assert_eq!(bytes, vec![byte as u8]),
                _ => unreachable!("the queue holds what was handed off, in order"),
            }
        }
    }

    #[tokio::test]
    async fn a_full_queue_is_what_stops_the_producer_reading_ahead() {
        let Wired {
            mut mailbox,
            mut incoming,
            consumed: _consumed,
        } = wired(SIZING.read_ahead);
        let cancel = CancellationToken::new();
        for byte in 0..SIZING.read_ahead {
            assert!(
                mailbox
                    .hand_off(Chunk::Payload(vec![byte as u8]), &cancel)
                    .await
            );
        }
        let blocked = mailbox.hand_off(Chunk::Payload(vec![0xff]), &cancel);
        let draining = async {
            // The owner taking one chunk is what frees the producer - the channel's own permit rather than
            // an acknowledgment or a marker of ours.
            tokio::task::yield_now().await;
            assert!(matches!(incoming.recv().await, Some(Chunk::Payload(_))));
        };
        let (handed, ()) = tokio::join!(blocked, draining);
        assert!(handed);
        assert_eq!(incoming.len(), SIZING.read_ahead);
    }

    #[tokio::test]
    async fn an_acknowledged_handover_still_waits_for_the_client_stack() {
        let Wired {
            mut mailbox,
            mut incoming,
            consumed,
        } = wired(SIZING.read_ahead);
        let cancel = CancellationToken::new();
        let handing = mailbox.hand_over(Chunk::Payload(vec![1]), &cancel);
        let owning = async {
            tokio::task::yield_now().await;
            assert!(matches!(incoming.recv().await, Some(Chunk::Payload(_))));
            // Taking it is deliberately not enough: the piece is alive until the owner says it was consumed,
            // which is what the resolver's delivery grant is written against.
            tokio::task::yield_now().await;
            consumed.send(()).await.expect("the producer is waiting");
        };
        let (handed, ()) = tokio::join!(handing, owning);
        assert!(handed);
    }

    #[tokio::test]
    async fn a_cancelled_producer_stops_waiting_for_room() {
        let Wired {
            mut mailbox,
            incoming: _incoming,
            consumed: _consumed,
        } = wired(1);
        let cancel = CancellationToken::new();
        assert!(mailbox.hand_off(Chunk::Payload(vec![1]), &cancel).await);
        let blocked = mailbox.hand_off(Chunk::Payload(vec![2]), &cancel);
        let retiring = async {
            tokio::task::yield_now().await;
            cancel.cancel();
        };
        let (handed, ()) = tokio::join!(blocked, retiring);
        assert!(!handed);
    }

    #[tokio::test]
    async fn a_cancelled_producer_stops_waiting_for_an_acknowledgment() {
        let Wired {
            mut mailbox,
            incoming: _incoming,
            consumed: _consumed,
        } = wired(SIZING.read_ahead);
        let cancel = CancellationToken::new();
        let waiting = mailbox.hand_over(Chunk::Payload(vec![1]), &cancel);
        let retiring = async {
            tokio::task::yield_now().await;
            cancel.cancel();
        };
        let (handed, ()) = tokio::join!(waiting, retiring);
        assert!(!handed, "the piece is the owner's to discard from here");
    }
}
