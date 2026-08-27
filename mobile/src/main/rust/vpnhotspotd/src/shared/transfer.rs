//! The owner's two ends of one flow's byte movement, and the one scan that decides what it must do next.
//!
//! `H` is whatever the owner names a flow's transport slot by - smoltcp's `SocketHandle` in the daemon - and
//! `P` is whatever one chunk of payload is carried as. What is here is the *decision*; the engine that owns a
//! stack, sockets and tasks wires it up and does the I/O.
//!
//! # Each queue is its own wake
//!
//! There is no readiness message between a flow's task and its owner, in either direction, and that is the
//! point:
//!
//! - **upstream to client**, the task queues a chunk and the queue's own receiver is what wakes the owner.
//!   The owner polls a flow's receiver only while that flow's row will accept a chunk; a busy row needs no
//!   wake at all, because consuming it refills the row synchronously from the same queue. So a burst is
//!   drained without a single wake after the first, and nothing is registered for a flow that could not use
//!   the answer.
//! - **client to upstream**, the owner holds a reserved slot and the task taking a chunk is what wakes it -
//!   see [crate::shared::room].
//!
//! What this replaces was one global readiness marker per queued chunk. That channel was prepared for one
//! marker per live flow, on the premise that duplicates coalesce - but they coalesce only *after* the owner
//! has received them, so a flow queueing its full read-ahead could put many markers in a channel every other
//! flow shares. A producer then blocked on a shared marker rather than on its own charged depth, which is the
//! scheduling dependency the read-ahead exists to remove, and with one prepared flow it blocked on the second
//! chunk of a queue forty-three deep.
//!
//! # One delivery per scan
//!
//! [poll_flows] moves at most one chunk into one row and answers with that flow's identity, so its owner can
//! refresh exactly the lifetime that saw traffic. Flows it did not reach keep whatever registration they had
//! and are found on the next scan, which the owner re-enters as soon as it has dealt with the answer. A flow
//! whose row it just filled is skipped by the next scan until that row is consumed, so no flow can hold the
//! scan against the others.

use std::hash::Hash;
use std::ops::Deref;
use std::task::Context;

use tokio::sync::mpsc;

use crate::shared::ended::Ended;
use crate::shared::fair::{FairQueue, FlowId};
use crate::shared::mailbox::Chunk;
use crate::shared::room::{Room, Unsent};

/// The owner's end of one flow: where its payload arrives, and its standing room in the flow's own queue.
pub struct Transfer<P> {
    /// The read-ahead queue this flow's producer queues into. Polled only while the row will take a chunk.
    chunks: mpsc::Receiver<Chunk<P>>,
    /// The reserved slot toward the flow's task. `None` once the client has half-closed or the task is gone:
    /// dropping it releases the reservation and closes the queue, which is how the half-close travels.
    upstream: Option<Room<P>>,
    /// Set once the producer's ordered end of stream has reached the row - which happens only after
    /// everything queued in front of it was consumed. The owner reads it to close the client's send side
    /// exactly once.
    finished: bool,
}

impl<P: Send + 'static> Transfer<P> {
    pub fn new(chunks: mpsc::Receiver<Chunk<P>>, upstream: Room<P>) -> Self {
        Self {
            chunks,
            upstream: Some(upstream),
            finished: false,
        }
    }

    /// Whether the producer's ordered end of stream has reached the row.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Whether the owner is holding this flow's upstream slot right now, asked without a waker for the paths
    /// that are deciding rather than waiting.
    pub fn reserved(&self) -> bool {
        self.upstream
            .as_ref()
            .is_some_and(|upstream| upstream.reserved())
    }

    /// Whether the owner still has an upstream slot at all, which is what says the client has not half-closed.
    pub fn sending(&self) -> bool {
        self.upstream.is_some()
    }

    /// Hands one chunk to this flow's task through the slot already held.
    ///
    /// `Err` carries the value back so the caller drops it: either no slot was in hand or the task is gone.
    pub fn send(&mut self, value: P) -> Result<(), Unsent<P>> {
        match self.upstream.as_mut() {
            Some(upstream) => upstream.send(value),
            None => Err(Unsent(Some(value))),
        }
    }

    /// The client half-closed, or this flow is being retired: the slot goes, and the queue closes with it so
    /// the task reads whatever is still queued and then sees the end of it.
    pub fn stop_sending(&mut self) {
        self.upstream = None;
    }

    /// Moves at most one queued chunk into this flow's row, keeping the row at one chunk.
    ///
    /// The consumption path, and the reason a busy row needs no wake: whenever the wire finishes a chunk its
    /// owner calls this, and what the producer queued behind it becomes the next row without anybody being
    /// woken. Taking one at a time is also what keeps the producer's order intact - an ordered end of stream
    /// cannot overtake payload queued in front of it, because that payload has to be consumed first.
    pub fn refill<H: Copy + Eq + Hash>(
        &mut self,
        id: FlowId<H>,
        fair: &mut FairQueue<H, P>,
    ) -> Refilled
    where
        P: Deref<Target = [u8]>,
    {
        if !fair.accepts(id) {
            return if self.chunks.is_empty() {
                Refilled::Idle
            } else {
                Refilled::Queued
            };
        }
        match self.chunks.try_recv() {
            Ok(chunk) => self.admit(id, fair, chunk),
            // Empty, or a producer that has finished and left nothing behind. Both are nothing to do.
            Err(_) => Refilled::Idle,
        }
    }

    /// Puts one taken chunk where it belongs, which is the one place payload and end of stream are told apart.
    fn admit<H: Copy + Eq + Hash>(
        &mut self,
        id: FlowId<H>,
        fair: &mut FairQueue<H, P>,
        chunk: Chunk<P>,
    ) -> Refilled
    where
        P: Deref<Target = [u8]>,
    {
        match chunk {
            Chunk::Payload(bytes) => match fair.deliver(id, bytes) {
                Ok(()) => Refilled::Payload,
                // The identity stopped accepting between the check above and here, which nothing can do while
                // this owner is the only writer - unwound anyway, because the alternative is a buffer this
                // process still holds and nobody counts.
                Err((bytes, _)) => {
                    drop(bytes);
                    Refilled::Stale
                }
            },
            Chunk::Finished => {
                fair.signal_eof(id);
                self.finished = true;
                Refilled::Finished
            }
        }
    }
}

/// What one attempt to move a queued chunk into the owner's row did.
#[derive(Debug, PartialEq, Eq)]
pub enum Refilled {
    /// One payload is now the row's, at offset zero.
    Payload,
    /// The producer's ordered end of stream reached the row. Everything it followed has already been
    /// consumed, because nothing is taken from the queue while the row still holds a chunk.
    Finished,
    /// The row would not take one - it still holds a chunk the wire has not finished with, or the identity is
    /// retiring - and this flow has more queued behind it. Real payload that has not been delivered yet,
    /// which is why it is told apart from [Refilled::Idle].
    Queued,
    /// Nothing to take: the queue is empty, the producer is gone, or the row cannot accept anything.
    Idle,
    /// This identity is retiring or was never registered, so the chunk taken for it was dropped here.
    Stale,
}

/// What a finished worker's terminal did to the flow it belonged to.
#[derive(Debug, PartialEq, Eq)]
pub enum Disposition<P> {
    /// The flow is handed on rather than ended, and **none of its state was touched**: its row still holds what
    /// the wire had not finished, its queue still holds what was behind that, and its identity is still one
    /// this owner may deliver to. The caller keeps the record and goes on draining.
    Detach,
    /// The flow is ending. Its row was discarded here, and whatever that row held comes back so the caller
    /// drops it - the same rule [FairQueue::begin_retire] follows, for the same reason: the owner drops its
    /// own buffers.
    Retire(Option<P>),
}

/// Decides what a terminal does to one flow, and discards its row only on the arm that decided to end it.
///
/// The order is the whole reason this is one function rather than two calls. A worker returns as soon as its
/// own ordered work is *queued*, so a terminal routinely arrives while the flow still owes its client a row's
/// worth of bytes and everything queued behind it. Discarding first and classifying afterwards would drop
/// exactly those bytes and the ordered end of stream with them - a client told nothing, waiting out its own
/// floor - and it is a two-line mistake to make at a call site. Here the discard is unreachable from the arm
/// that hands the flow on.
///
/// `cancelled` is whether somebody asked this worker to stop and `opened` whether the client's own half ever
/// got past its handshake and is not already closed; both are [Ended::detaches]'s.
pub fn dispose<H: Copy + Eq + Hash, P: Deref<Target = [u8]>>(
    ended: &Ended,
    cancelled: bool,
    opened: bool,
    id: FlowId<H>,
    fair: &mut FairQueue<H, P>,
) -> Disposition<P> {
    if ended.detaches(cancelled, opened) {
        return Disposition::Detach;
    }
    Disposition::Retire(fair.begin_retire(id))
}

/// One flow, as a scan sees it.
pub struct Polling<'a, H, P> {
    pub id: FlowId<H>,
    pub transfer: &'a mut Transfer<P>,
    /// Whether the client-side stack has bytes this flow's task could take right now. Read by the owner from
    /// its own stack, because that is the half this module does not own.
    pub receiving: bool,
}

/// What one scan found for the owner to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Ready<H> {
    /// One flow's queued payload - or its ordered end of stream - is now in that flow's row. The identity is
    /// what the owner refreshes the lifetime of, so it names the flow rather than travelling payload-free.
    Delivered(FlowId<H>),
    /// At least one flow is holding its upstream slot and has bytes waiting in the stack, so moving them is
    /// work the owner can do now. Payload-free, because what to do about it is decided from owner state.
    Upstream,
    /// Nothing to do. Every flow that could use a wake is registered for one.
    Nothing,
}

/// Polls every flow once: registers each one that could still be told something, and answers with the first
/// thing the owner can act on.
///
/// A row that will not take a chunk is not polled at all, which is deliberate rather than an optimisation: a
/// wake for a flow whose row is busy would be a wake the owner could not use, and the consumption that frees
/// that row refills it from the same queue without one. A flow whose producer is gone answers immediately and
/// forever, so a drained queue cannot spin this either.
///
/// Answering a delivery stops the scan, so the rooms after it keep the registration they already had and are
/// polled again on the next scan - which the owner re-enters directly, exactly as a biased `select!` re-polls
/// the arms it skipped.
pub fn poll_flows<'a, H: Copy + Eq + Hash, P: Deref<Target = [u8]> + Send + 'static>(
    cx: &mut Context<'_>,
    flows: impl Iterator<Item = Polling<'a, H, P>>,
    fair: &mut FairQueue<H, P>,
) -> Ready<H> {
    let mut upstream = false;
    for Polling {
        id,
        transfer,
        receiving,
    } in flows
    {
        // Asked first, and for every flow: polling is what registers this owner for the moment the flow's
        // task frees the slot, and a flow passed over would be one whose queue could empty with nobody
        // listening.
        if let Some(room) = transfer.upstream.as_mut() {
            if room.poll_reserve(cx) && receiving {
                upstream = true;
            }
        }
        if !fair.accepts(id) {
            continue;
        }
        // Ready means a chunk was taken, so it has to go somewhere this poll: it is out of the queue and the
        // row is the only other place it may be.
        let std::task::Poll::Ready(Some(chunk)) = transfer.chunks.poll_recv(cx) else {
            continue;
        };
        transfer.admit(id, fair, chunk);
        return Ready::Delivered(id);
    }
    if upstream {
        Ready::Upstream
    } else {
        Ready::Nothing
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Wake, Waker};

    use tokio_util::sync::CancellationToken;

    use super::*;

    use crate::shared::fair::Progress;
    use crate::shared::flow_budget::SIZING;
    use crate::shared::mailbox::Mailbox;

    /// A waker that only counts, so a test can say whether *this queue* woke the owner rather than whether
    /// something else in the process happened to.
    #[derive(Default)]
    struct Counting(AtomicUsize);

    impl Wake for Counting {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn counting() -> (Arc<Counting>, Waker) {
        let counter = Arc::new(Counting::default());
        let waker = Waker::from(Arc::clone(&counter));
        (counter, waker)
    }

    /// One flow wired exactly as production wires it: the producer's mailbox, the owner's transfer, and the
    /// task's end of the upstream queue.
    struct Wired {
        id: FlowId<u32>,
        mailbox: Mailbox<u32, Vec<u8>>,
        transfer: Transfer<Vec<u8>>,
        task: mpsc::Receiver<Vec<u8>>,
        /// The owner's acknowledgment end, which nothing here reads and every test has to keep: a producer
        /// answers `false` for an owner that is *gone* exactly as it does for a cancellation, and only a
        /// DNS-over-TCP transport waits on this one.
        _consumed: mpsc::Sender<()>,
    }

    fn wired(handle: u32, worker: u64) -> Wired {
        let id = FlowId::new(handle, worker);
        // The depths production builds: the read-ahead queue toward the client, one slot toward the task.
        let (chunks, incoming) = mpsc::channel(SIZING.read_ahead);
        let (consumed, acknowledged) = mpsc::channel(SIZING.control);
        let (upstream, task) = mpsc::channel(SIZING.control);
        Wired {
            id,
            mailbox: Mailbox {
                chunks,
                consumed: acknowledged,
                identity: id,
            },
            transfer: Transfer::new(incoming, Room::new(upstream)),
            task,
            _consumed: consumed,
        }
    }

    fn queue(prepared: usize, ids: &[FlowId<u32>]) -> FairQueue<u32, Vec<u8>> {
        let mut fair = FairQueue::with_capacity(prepared);
        for id in ids {
            fair.admit(*id).expect("the queue was prepared for it");
        }
        fair
    }

    /// The production scan, over the flows a test wired, with `receiving` standing in for what the owner
    /// reads from its own stack.
    fn scan(
        cx: &mut Context<'_>,
        flows: &mut [(&mut Wired, bool)],
        fair: &mut FairQueue<u32, Vec<u8>>,
    ) -> Ready<u32> {
        poll_flows(
            cx,
            flows.iter_mut().map(|(wired, receiving)| Polling {
                id: wired.id,
                transfer: &mut wired.transfer,
                receiving: *receiving,
            }),
            fair,
        )
    }

    #[tokio::test]
    async fn a_queued_chunk_wakes_the_owner_through_its_own_queue() {
        let mut flow = wired(7, 100);
        let mut fair = queue(4, &[flow.id]);
        let (woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        // Nothing to do yet, and the scan is what registers this owner with the flow's own queue.
        assert_eq!(
            scan(&mut cx, &mut [(&mut flow, false)], &mut fair),
            Ready::Nothing
        );
        assert_eq!(woken.0.load(Ordering::Relaxed), 0);
        let cancel = CancellationToken::new();
        assert!(
            flow.mailbox
                .hand_off(Chunk::Payload(vec![1, 2]), &cancel)
                .await
        );
        // The enqueue alone made the owner runnable: no marker travelled, and no other flow was involved.
        assert_eq!(woken.0.load(Ordering::Relaxed), 1);
        assert_eq!(
            scan(&mut cx, &mut [(&mut flow, false)], &mut fair),
            Ready::Delivered(flow.id)
        );
        assert_eq!(fair.peek(flow.id), Some(&[1, 2][..]));
    }

    #[tokio::test]
    async fn a_busy_row_is_not_polled_and_needs_no_wake() {
        let mut flow = wired(7, 100);
        let mut fair = queue(4, &[flow.id]);
        let (woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        let cancel = CancellationToken::new();
        assert!(
            flow.mailbox
                .hand_off(Chunk::Payload(vec![1]), &cancel)
                .await
        );
        assert_eq!(
            scan(&mut cx, &mut [(&mut flow, false)], &mut fair),
            Ready::Delivered(flow.id)
        );
        let before = woken.0.load(Ordering::Relaxed);
        // The row is busy, so the scan does not poll the queue - and the producer queueing behind it wakes
        // nobody, because a wake is something this owner could not use yet.
        assert_eq!(
            scan(&mut cx, &mut [(&mut flow, false)], &mut fair),
            Ready::Nothing
        );
        assert!(
            flow.mailbox
                .hand_off(Chunk::Payload(vec![2]), &cancel)
                .await
        );
        assert_eq!(woken.0.load(Ordering::Relaxed), before);
        // Consuming the row is what takes the next chunk, synchronously and without a wake.
        let mut round = fair.begin_round();
        assert_eq!(fair.next(&mut round), Some(flow.id));
        assert_eq!(fair.serviced(flow.id, 1), Progress::Consumed);
        assert_eq!(flow.transfer.refill(flow.id, &mut fair), Refilled::Payload);
        assert_eq!(fair.peek(flow.id), Some(&[2][..]));
    }

    #[tokio::test]
    async fn one_delivery_per_scan_and_every_flow_gets_its_turn() {
        let mut first = wired(7, 100);
        let mut second = wired(8, 101);
        let mut fair = queue(4, &[first.id, second.id]);
        let (_woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        let cancel = CancellationToken::new();
        assert!(
            first
                .mailbox
                .hand_off(Chunk::Payload(vec![1]), &cancel)
                .await
        );
        assert!(
            second
                .mailbox
                .hand_off(Chunk::Payload(vec![2]), &cancel)
                .await
        );
        let mut delivered = Vec::new();
        for _ in 0..2 {
            match scan(
                &mut cx,
                &mut [(&mut first, false), (&mut second, false)],
                &mut fair,
            ) {
                Ready::Delivered(id) => delivered.push(id),
                other => unreachable!("both flows have payload waiting, not {other:?}"),
            }
        }
        // One at a time, and both of them: a flow whose row was just filled is skipped rather than taking the
        // scan again.
        assert_eq!(delivered.len(), 2);
        assert!(delivered.contains(&first.id) && delivered.contains(&second.id));
        assert_eq!(
            scan(
                &mut cx,
                &mut [(&mut first, false), (&mut second, false)],
                &mut fair
            ),
            Ready::Nothing
        );
    }

    #[tokio::test]
    async fn capacity_restored_by_the_task_is_what_makes_the_owner_runnable() {
        let mut flow = wired(7, 100);
        let mut fair = queue(4, &[flow.id]);
        let (woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        // The owner takes its slot and fills the depth-one queue, which is the state a client upload reaches.
        assert_eq!(
            scan(&mut cx, &mut [(&mut flow, true)], &mut fair),
            Ready::Upstream
        );
        assert!(flow.transfer.reserved());
        assert!(flow.transfer.send(vec![1]).is_ok());
        // No room, so the scan registers and answers nothing - with the stack still holding bytes to move.
        assert_eq!(
            scan(&mut cx, &mut [(&mut flow, true)], &mut fair),
            Ready::Nothing
        );
        assert!(!flow.transfer.reserved());
        assert_eq!(woken.0.load(Ordering::Relaxed), 0);
        // The task taking the chunk is the only thing that happens: no packet, no timer, no config, no other
        // flow.
        assert_eq!(flow.task.recv().await, Some(vec![1]));
        assert_eq!(woken.0.load(Ordering::Relaxed), 1);
        // ...and that alone is what lets the owner drain the next bytes.
        assert_eq!(
            scan(&mut cx, &mut [(&mut flow, true)], &mut fair),
            Ready::Upstream
        );
        assert!(flow.transfer.send(vec![2]).is_ok());
        assert_eq!(flow.task.recv().await, Some(vec![2]));
    }

    #[tokio::test]
    async fn a_stack_with_nothing_to_move_is_not_answered_as_work() {
        let mut flow = wired(7, 100);
        let mut fair = queue(4, &[flow.id]);
        let (_woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        // A slot in hand with nothing to put in it is kept rather than reported, so an owner polling this in
        // a loop is not spun by a flow that has nothing to send.
        assert_eq!(
            scan(&mut cx, &mut [(&mut flow, false)], &mut fair),
            Ready::Nothing
        );
        assert!(flow.transfer.reserved());
        assert_eq!(
            scan(&mut cx, &mut [(&mut flow, false)], &mut fair),
            Ready::Nothing
        );
        assert!(flow.transfer.reserved());
    }

    #[tokio::test]
    async fn a_clean_terminal_leaves_queued_payload_and_the_end_of_stream_to_be_delivered() {
        let Wired {
            id,
            mut mailbox,
            mut transfer,
            task: _task,
            _consumed,
        } = wired(7, 100);
        let mut fair = queue(4, &[id]);
        let (_woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        let cancel = CancellationToken::new();
        // Exactly what an ordinary worker does before it returns cleanly: queue the payload, queue the
        // ordered end of stream, and go. Nothing waits for the client's stack to have taken them, so the
        // terminal its owner sees arrives with both still here - and the owner detaches rather than
        // discarding them, which is [crate::shared::ended::Ended::detaches].
        assert!(
            mailbox
                .hand_off(Chunk::Payload(vec![1, 2, 3]), &cancel)
                .await
        );
        assert!(mailbox.hand_off(Chunk::Finished, &cancel).await);
        drop(mailbox);
        let mut scan = |transfer: &mut Transfer<Vec<u8>>, fair: &mut FairQueue<u32, Vec<u8>>| {
            poll_flows(
                &mut cx,
                std::iter::once(Polling {
                    id,
                    transfer,
                    receiving: false,
                }),
                fair,
            )
        };
        // The owner delivers the payload first...
        assert_eq!(scan(&mut transfer, &mut fair), Ready::Delivered(id));
        assert_eq!(fair.peek(id), Some(&[1, 2, 3][..]));
        assert!(
            !transfer.finished(),
            "the end of stream is still queued behind the payload"
        );
        let mut round = fair.begin_round();
        assert_eq!(fair.next(&mut round), Some(id));
        assert_eq!(fair.serviced(id, 3), Progress::Consumed);
        // ...and only then the end of stream, which is what closes the client's own half.
        assert_eq!(transfer.refill(id, &mut fair), Refilled::Finished);
        assert!(transfer.finished());
        assert_eq!(fair.serviced(id, 0), Progress::Eof);
        // Nothing is left: the producer is gone and its queue is drained.
        assert_eq!(scan(&mut transfer, &mut fair), Ready::Nothing);
    }

    #[tokio::test]
    async fn a_clean_terminal_disposes_of_the_flow_before_anything_of_its_is_discarded() {
        let Wired {
            id,
            mut mailbox,
            mut transfer,
            task: _task,
            _consumed,
        } = wired(7, 100);
        let mut fair = queue(4, &[id]);
        let (_woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        let cancel = CancellationToken::new();
        // The state a worker really leaves behind when it ends cleanly: one chunk in the row, the ordered end
        // of stream queued behind it, and the producer gone.
        assert!(
            mailbox
                .hand_off(Chunk::Payload(vec![1, 2, 3]), &cancel)
                .await
        );
        assert!(mailbox.hand_off(Chunk::Finished, &cancel).await);
        drop(mailbox);
        assert_eq!(
            poll_flows(
                &mut cx,
                std::iter::once(Polling {
                    id,
                    transfer: &mut transfer,
                    receiving: false,
                }),
                &mut fair
            ),
            Ready::Delivered(id)
        );
        // The terminal as production classifies it: a clean ending, nobody cancelled it, the client's half is
        // open.
        assert_eq!(
            dispose(&Ended::Expected, false, true, id, &mut fair),
            Disposition::Detach
        );
        // Nothing of the flow's was retired or discarded by that decision.
        assert!(
            fair.is_admitted(id),
            "the identity may still be delivered to"
        );
        assert_eq!(fair.peek(id), Some(&[1, 2, 3][..]));
        assert!(fair.owes(id));
        // ...and the owner goes on delivering: the payload it already had, and then the end of stream that
        // was queued behind it.
        let mut round = fair.begin_round();
        assert_eq!(fair.next(&mut round), Some(id));
        assert_eq!(fair.serviced(id, 3), Progress::Consumed);
        assert_eq!(transfer.refill(id, &mut fair), Refilled::Finished);
        assert!(transfer.finished());
        assert_eq!(fair.serviced(id, 0), Progress::Eof);
        assert!(!fair.owes(id));
    }

    #[tokio::test]
    async fn an_ending_that_is_not_a_clean_completion_discards_the_row_it_ends() {
        let Wired {
            id,
            mut mailbox,
            mut transfer,
            task: _task,
            _consumed,
        } = wired(7, 100);
        let mut fair = queue(4, &[id]);
        let (_woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        let cancel = CancellationToken::new();
        assert!(mailbox.hand_off(Chunk::Payload(vec![9; 4]), &cancel).await);
        assert_eq!(
            poll_flows(
                &mut cx,
                std::iter::once(Polling {
                    id,
                    transfer: &mut transfer,
                    receiving: false,
                }),
                &mut fair
            ),
            Ready::Delivered(id)
        );
        // A peer that reset is not a clean completion: the flow ends, and what it owed comes back to the
        // caller to drop rather than being dropped here.
        match dispose(
            &Ended::Reported("a peer that reset".to_owned()),
            false,
            true,
            id,
            &mut fair,
        ) {
            Disposition::Retire(Some(discarded)) => assert_eq!(discarded, vec![9; 4]),
            other => unreachable!("an ending discards the row it ends, not {other:?}"),
        }
        assert!(!fair.is_admitted(id));
        assert_eq!(fair.peek(id), None);
        assert!(!fair.owes(id));
    }

    #[tokio::test]
    async fn a_retiring_identity_is_never_delivered_to() {
        let mut flow = wired(7, 100);
        let mut fair = queue(4, &[flow.id]);
        let (_woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        let cancel = CancellationToken::new();
        assert!(
            flow.mailbox
                .hand_off(Chunk::Payload(vec![4]), &cancel)
                .await
        );
        // Exactly what a retirement does before it cancels the worker.
        assert!(fair.begin_retire(flow.id).is_none());
        assert_eq!(
            scan(&mut cx, &mut [(&mut flow, false)], &mut fair),
            Ready::Nothing
        );
        assert_eq!(fair.peek(flow.id), None);
        assert_eq!(flow.transfer.refill(flow.id, &mut fair), Refilled::Queued);
        // ...and so is a flow this queue never admitted, which is what a reused handle looks like from the
        // owner's side: the same slot, a successor's identity.
        let stale = FlowId::new(7, 101);
        assert!(!fair.is_admitted(stale));
        assert_eq!(
            poll_flows(
                &mut cx,
                std::iter::once(Polling {
                    id: stale,
                    transfer: &mut flow.transfer,
                    receiving: false,
                }),
                &mut fair
            ),
            Ready::Nothing
        );
    }

    #[tokio::test]
    async fn the_half_close_releases_the_slot_and_closes_the_queue() {
        let mut flow = wired(7, 100);
        let mut fair = queue(4, &[flow.id]);
        let (_woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(
            scan(&mut cx, &mut [(&mut flow, true)], &mut fair),
            Ready::Upstream
        );
        assert!(flow.transfer.send(vec![1]).is_ok());
        // The client half-closed: the slot goes, and what was queued still reaches the task before the end.
        flow.transfer.stop_sending();
        assert!(!flow.transfer.sending());
        assert_eq!(flow.task.recv().await, Some(vec![1]));
        assert_eq!(flow.task.recv().await, None);
        // Nothing is polled for a flow with no slot, and a send after it is refused rather than queued.
        assert_eq!(
            scan(&mut cx, &mut [(&mut flow, true)], &mut fair),
            Ready::Nothing
        );
        match flow.transfer.send(vec![2]) {
            Err(Unsent(Some(value))) => assert_eq!(value, vec![2]),
            _ => unreachable!("a flow with no slot hands the chunk back"),
        }
    }
}
