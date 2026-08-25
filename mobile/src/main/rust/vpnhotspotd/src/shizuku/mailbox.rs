//! One flow's depth-one payload mailbox, and the sequential handover that keeps it at depth one.
//!
//! `H` is whatever the owner names a flow's transport slot by - smoltcp's `SocketHandle` in the daemon - so
//! the ownership rule does not depend on the stack it serves.
//!
//! # Depth one means one chunk exists
//!
//! Not "one is queued". The producer hands a chunk over and then waits for the *consumption* acknowledgment
//! before it builds the next, so at any moment exactly one chunk allocation is alive for a flow. A producer
//! that built its pieces up front and handed them over afterwards would satisfy the queue's depth and violate
//! the bound the depth exists to express: the whole response would be alive at once, in as many allocations
//! as the read quantum divides into it.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::shizuku::owned;
use vpnhotspotd::shared::fair::FlowId;
use vpnhotspotd::shared::preempt::hand_over;

/// What one flow's producer puts in its mailbox.
pub(crate) enum Chunk {
    /// Bytes for the client-side stack, in order.
    Payload(Payload),
    /// The producer finished sending. The client-side half-close follows, not a reset: a stream that ended
    /// cleanly upstream must end cleanly downstream too. Ordered strictly after whatever payload preceded it.
    Finished,
}

/// One piece of payload on its way to a client's stack, counted for exactly as long as it exists.
///
/// The count travels with the buffer rather than staying with whoever built it. A piece still sitting in a
/// mailbox after its producer was cancelled is a piece this process still owns, and the depth-one bound is a
/// statement about buffers that exist rather than about buffers someone is still waiting on.
pub(crate) type Payload = owned::Owned;

/// One flow's end of the arrangement: where its chunks go, how it is woken, and how it learns one was
/// consumed.
pub(crate) struct Mailbox<H> {
    pub(crate) chunks: mpsc::Sender<Chunk>,
    pub(crate) ready: mpsc::Sender<FlowId<H>>,
    /// Signalled by the owner when a chunk has been *consumed* - fully written into the client's send buffer,
    /// not merely delivered. Waiting for it is what makes depth one mean depth one: without it a second chunk
    /// could be built while the first was still being written, which is two chunks of buffer per flow under
    /// exactly the conditions that made the bound necessary.
    pub(crate) consumed: mpsc::Receiver<()>,
    pub(crate) identity: FlowId<H>,
}

impl<H: Copy> Mailbox<H> {
    /// Hands one chunk over and waits for the owner to consume it, or for the flow to be cancelled.
    ///
    /// `false` means cancelled: the owner has discarded this identity's pending payload and the wait is over
    /// whether or not anything was consumed.
    pub(crate) async fn hand_over(&mut self, chunk: Chunk, cancel: &CancellationToken) -> bool {
        // The mailbox is empty by construction - nothing else produces into it, and the previous chunk was
        // acknowledged before this one was built - so this send does not block. Racing the token anyway costs
        // nothing and keeps the invariant from being load-bearing.
        if !hand_over(&self.chunks, chunk, cancel).await {
            return false;
        }
        if !hand_over(&self.ready, self.identity, cancel).await {
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
pub(crate) enum Handed {
    /// Every piece was handed over and acknowledged.
    Complete,
    /// Cancelled part-way. Whatever was in flight is the owner's to discard.
    Cancelled,
}
