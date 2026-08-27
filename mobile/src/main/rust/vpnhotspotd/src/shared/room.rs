//! One owner's standing room in a worker's bounded queue, and the wake that says the room is back.
//!
//! # The wake is the whole point
//!
//! An owner that feeds many workers from one task has to decide, per worker, whether it may hand over the
//! next value - and if it may not, it has to be told when it may. Asking the channel how much capacity it has
//! answers the first question and not the second: capacity read that way is a number that changed a moment
//! later, and nothing registers the owner to be woken when the consumer takes something out. An owner that
//! stopped draining its own source on such a reading stays stopped until some unrelated event happens to wake
//! it, which for a terminated TCP flow is the client's window left closed until a zero-window probe.
//!
//! So the owner holds a **reserved slot** instead of reading a number. [Room::poll_reserve] is the library's
//! own readiness registration - `tokio_util`'s `PollSender`, and under it the channel's semaphore - so the
//! consumer taking a value is what wakes the owner, with no message of ours in either direction and no
//! capacity state kept twice. A slot in hand is the owner's permission to hand over exactly one value; using
//! it starts the next reservation, which is what re-registers the wake.
//!
//! # What it costs
//!
//! One slot of the queue's own depth while it is held, and no value: a reservation is a semaphore permit, so
//! the bytes an owner may have in flight toward one worker are the depth it was charged for and not one
//! more. Beyond the channel, `PollSender` keeps one boxed permit-acquisition future - see
//! [ACQUIRE_FUTURE_BYTES], which is what an owner charges for it.
//!
//! Dropping a room releases whatever it holds and drops the sending half with it, so an owner signalling a
//! half-close by dropping this closes the worker's queue exactly as dropping a bare sender did - values
//! already queued still reach the worker first.

use std::task::{Context, Poll};

use tokio::sync::mpsc;
use tokio_util::sync::PollSender;

/// An upper bound on the one heap allocation a room keeps beyond the channel it reserves in.
///
/// `PollSender` holds its permit acquisition in a `ReusableBoxFuture`, which is a boxed future whose state is
/// a `Sender<T>` and the channel semaphore's `Acquire` - a waiter node with a waker in it
/// (`tokio-util 0.7.19`, `sync/mpsc.rs:56-107`). That layout is crate-private, so this is deliberately far
/// more than it needs rather than a reproduction of it: charged once per room, a generous figure costs a few
/// kilobytes across a whole dataplane and buys a bound that does not have to be revisited when a runtime
/// version reorganises itself. Bumping `tokio-util` means re-reading that file, exactly as
/// [crate::shared::reply_bound]'s channel figures do.
pub const ACQUIRE_FUTURE_BYTES: u64 = 512;

/// Nothing was queued, and the value comes back so the caller drops it rather than this module.
///
/// `None` only where the runtime kept the value, which `PollSender` does for one it has already taken.
#[derive(Debug)]
pub struct Unsent<T>(pub Option<T>);

/// The owner's end of one worker's queue: the slot it is holding, or the registration that will give it one.
pub struct Room<T> {
    slot: PollSender<T>,
    /// Whether a slot is in hand right now. Kept here because `PollSender` does not answer it, and it is what
    /// keeps [Room::send] from reaching the panic that library raises for a send with no reservation - this
    /// daemon aborts on panic, so an unreachable panic is still a process this device loses.
    reserved: bool,
}

impl<T: Send + 'static> Room<T> {
    pub fn new(sender: mpsc::Sender<T>) -> Self {
        Self {
            slot: PollSender::new(sender),
            reserved: false,
        }
    }

    /// Whether the owner may hand one value over right now, asked without a waker for the paths that are
    /// deciding rather than waiting.
    pub fn reserved(&self) -> bool {
        self.reserved
    }

    /// Takes the slot if the queue has room, and otherwise registers `cx`'s task to be woken when the worker
    /// frees one.
    ///
    /// `false` covers both "not yet" and "never again": a queue whose worker is gone answers immediately and
    /// stays answered, so an owner polling this in a loop is not spun by a dead flow. A slot already in hand
    /// is answered without touching the channel, which is what keeps a repeated poll from reserving twice.
    pub fn poll_reserve(&mut self, cx: &mut Context<'_>) -> bool {
        if self.reserved {
            return true;
        }
        match self.slot.poll_reserve(cx) {
            Poll::Ready(Ok(())) => {
                self.reserved = true;
                true
            }
            // The worker's receiving half is gone. Permanent, and the flow's own ending is what settles it.
            Poll::Ready(Err(_)) => false,
            Poll::Pending => false,
        }
    }

    /// Hands one value over through the slot already held, and starts the next reservation by giving the slot
    /// up.
    ///
    /// The next [Room::poll_reserve] is what re-registers the wake, so an owner that sends and then polls is
    /// never left waiting on a capacity nobody will tell it about.
    pub fn send(&mut self, value: T) -> Result<(), Unsent<T>> {
        if !self.reserved {
            return Err(Unsent(Some(value)));
        }
        self.reserved = false;
        // A reservation outlives the worker that would have taken what it holds, and a permit sent into a
        // queue whose receiver is gone is *accepted*: the value is queued into a channel nobody will read
        // and the send answers `Ok`, which is a value lost without a word. Asked here so the caller learns,
        // and the slot given up and this end closed with it - otherwise the owner would go on reserving and
        // reporting for a worker that no longer exists.
        if self.slot.get_ref().is_some_and(|queue| queue.is_closed()) {
            self.slot.abort_send();
            self.slot.close();
            return Err(Unsent(Some(value)));
        }
        self.slot
            .send_item(value)
            .map_err(|refused| Unsent(refused.into_inner()))
    }

    /// Whether the worker's queue is gone, which is what a failed reservation means for good.
    pub fn closed(&self) -> bool {
        self.slot.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Wake, Waker};

    use super::*;

    /// A waker that only counts, so a test can say whether the *channel* woke the owner rather than whether
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

    #[tokio::test]
    async fn the_worker_freeing_the_queue_is_what_wakes_the_owner() {
        let (sender, mut worker) = mpsc::channel(1);
        let mut room = Room::new(sender);
        let (woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        // One slot, taken and used, which is exactly the state an owner reaches when it has just handed a
        // chunk over.
        assert!(room.poll_reserve(&mut cx));
        assert!(room.send(1u8).is_ok());
        // The queue is full, so the owner has no room and is registered for the moment it does. Nothing else
        // is running: no timer, no unrelated traffic, no message from the worker.
        assert!(!room.poll_reserve(&mut cx));
        assert!(!room.reserved());
        assert_eq!(woken.0.load(Ordering::Relaxed), 0);
        assert_eq!(worker.recv().await, Some(1u8));
        // Capacity restored by the consumer alone, and the owner is runnable because of it.
        assert_eq!(woken.0.load(Ordering::Relaxed), 1);
        assert!(room.poll_reserve(&mut cx));
        assert!(room.send(2u8).is_ok());
        assert_eq!(worker.recv().await, Some(2u8));
    }

    #[tokio::test]
    async fn a_reservation_is_a_slot_rather_than_a_queued_value() {
        let (sender, worker) = mpsc::channel(2);
        let mut room = Room::new(sender);
        let (_woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        assert!(room.poll_reserve(&mut cx));
        // Holding it takes one of the depth the owner was charged for, and puts no value in the queue.
        assert!(worker.is_empty());
        assert_eq!(worker.len(), 0);
        // Polling again while holding one reserves nothing further, so the peak is the depth and not the
        // number of polls.
        assert!(room.poll_reserve(&mut cx));
        assert!(room.send(9u8).is_ok());
        assert_eq!(worker.len(), 1);
    }

    #[tokio::test]
    async fn a_send_with_no_slot_in_hand_is_refused_rather_than_a_panic() {
        let (sender, _worker) = mpsc::channel::<u8>(1);
        let mut room = Room::new(sender);
        match room.send(3) {
            Err(Unsent(Some(3))) => {}
            _ => unreachable!("an unreserved send hands the value back"),
        }
    }

    #[tokio::test]
    async fn dropping_the_room_closes_the_queue_and_lets_what_was_queued_arrive_first() {
        let (sender, mut worker) = mpsc::channel(2);
        let mut room = Room::new(sender);
        let (_woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        assert!(room.poll_reserve(&mut cx));
        assert!(room.send(1u8).is_ok());
        // A second slot is in hand when the owner half-closes, which is the case that would strand a
        // reservation if dropping it did not release one.
        assert!(room.poll_reserve(&mut cx));
        drop(room);
        assert_eq!(worker.recv().await, Some(1u8));
        assert_eq!(worker.recv().await, None);
    }

    #[tokio::test]
    async fn a_worker_that_goes_while_the_slot_is_in_hand_is_answered_rather_than_queued() {
        let (sender, worker) = mpsc::channel(1);
        let mut room = Room::new(sender);
        let (_woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        assert!(room.poll_reserve(&mut cx));
        // Exactly the window between a worker task ending and its terminal reaching the owner: the slot was
        // taken while the queue still had a reader, and the reader is gone by the time it is used.
        drop(worker);
        match room.send(7u8) {
            Err(Unsent(Some(7))) => {}
            _ => unreachable!(
                "a value that cannot reach a worker comes back rather than being queued"
            ),
        }
        // Permanent afterwards, so the owner stops reserving for a flow that has no reader left.
        assert!(room.closed());
        assert!(!room.poll_reserve(&mut cx));
    }

    #[tokio::test]
    async fn a_worker_that_is_gone_is_answered_rather_than_waited_on() {
        let (sender, worker) = mpsc::channel::<u8>(1);
        let mut room = Room::new(sender);
        drop(worker);
        let (woken, waker) = counting();
        let mut cx = Context::from_waker(&waker);
        assert!(!room.poll_reserve(&mut cx));
        assert!(room.closed());
        // Answered immediately and permanently, so an owner polling every flow is not spun by this one.
        assert!(!room.poll_reserve(&mut cx));
        assert_eq!(woken.0.load(Ordering::Relaxed), 0);
        match room.send(1) {
            Err(Unsent(Some(1))) => {}
            _ => unreachable!("a closed queue hands the value back"),
        }
    }
}
