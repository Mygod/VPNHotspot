//! Conservative memory bounds for reply channels and error-queue drains.
use crate::shared::admission::linear_footprint;

/// The value slots one charged block covers: the *largest* `BLOCK_CAP` across the pinned targets.
const CHARGED_BLOCK_VALUES: u64 = 32;

/// The divisor used to decide how many blocks a depth needs: the *smallest* `BLOCK_CAP` across the pinned
/// targets.
const VALUES_PER_BLOCK: u64 = 16;

/// Blocks the *recycling* may hold beyond the ones a channel's depth needs.
const SPARE_BLOCKS: u64 = 3;

/// An upper bound on one block's own overhead beyond the values in it: its link pointer, its start index, and
/// the atomic ready/observed bitfields it keeps per slot.
const BLOCK_HEADER_BYTES: u64 = (std::mem::size_of::<usize>() as u64) * 6 * 2;

/// An upper bound on the shared channel state: the reference-counted `Chan`, its semaphore, the two waker
/// slots, the receiver's cursor and the sender/receiver handles the owner retains.
const CHANNEL_STATE_BYTES: u64 = 1024;

/// How many blocks a channel of `depth` values with `producers` senders may hold at once.
fn blocks_for(depth: u64, producers: u64) -> Option<u64> {
    depth
        .checked_add(VALUES_PER_BLOCK - 1)?
        .checked_div(VALUES_PER_BLOCK)?
        .checked_add(producers.min(depth))?
        .checked_add(SPARE_BLOCKS)
}

/// A conservative upper bound on everything a bounded channel of `depth` values of `T` retains, for an owner
/// that may have `producers` senders inside `send` at once.
pub fn channel_footprint<T>(depth: usize, producers: usize) -> Option<u64> {
    let depth = u64::try_from(depth).ok()?;
    let producers = u64::try_from(producers).ok()?;
    let per_block = linear_footprint(
        CHARGED_BLOCK_VALUES as usize,
        std::mem::size_of::<T>() as u64,
    )?
    .checked_add(BLOCK_HEADER_BYTES)?
    // One alignment's worth per block, which covers the padding an over-aligned `T` forces between the
    // four-word header and the value array. A zero-sized `T` charges the header and this alone, which is
    // still a real allocation and still a real block.
    .checked_add(std::mem::align_of::<T>() as u64)?;
    blocks_for(depth, producers)?
        .checked_mul(per_block)?
        .checked_add(CHANNEL_STATE_BYTES)
}

/// A bounded reply channel's whole retained cost: its own allocation, and the payloads its slots may carry.
pub fn reply_channel_footprint<T>(depth: usize, producers: usize, max_payload: u64) -> Option<u64> {
    let depth = u64::try_from(depth).ok()?;
    channel_footprint::<T>(depth as usize, producers)?
        .checked_add(depth.checked_add(1)?.checked_mul(max_payload)?)
}

/// The depth a channel is really built at, for a requested capacity.
pub fn built_depth(requested: usize) -> usize {
    requested.max(1)
}

/// What one message off a socket's error queue is, as far as the decision below is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    /// An ICMP error a router sent, which the owner may be able to correlate.
    Remote,
    /// The kernel's own refusal of a send this worker did not make.
    Local,
    /// A message that named neither.
    Neither,
    /// The queue is empty.
    Empty,
}

/// What a worker does with the channel slot it is holding, for exactly one drained message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Send the error through the slot already held. No second slot is taken and nothing waits.
    Send,
    /// Give the slot back and go round again. The socket stays error-readable if more remain, so the next
    /// turn takes a fresh slot for the next message.
    Release,
}

/// What one readiness turn produced, and therefore what becomes of the slot it is holding.
#[derive(Debug)]
pub enum Readiness<T> {
    /// A datagram, already allocated, to go through the slot already held.
    Received(T),
    /// The readiness was stale - the read would have blocked. The slot goes back and the socket is waited on
    /// again.
    Stale,
    /// The socket has a queued kernel error instead of a datagram. The same slot serves the error path.
    Errored,
    /// The daemon's own I/O went wrong, which ends this worker.
    Failed(std::io::Error),
}

/// Classifies one attempted read, from exactly the shape `AsyncFd::try_io` answers with.
pub fn classify_readiness<T, W>(
    attempt: Result<std::io::Result<T>, W>,
    kernel_error: impl Fn(&std::io::Error) -> bool,
) -> Readiness<T> {
    match attempt {
        Ok(Ok(received)) => Readiness::Received(received),
        Ok(Err(e)) if kernel_error(&e) => Readiness::Errored,
        Ok(Err(e)) => Readiness::Failed(e),
        Err(_would_block) => Readiness::Stale,
    }
}

/// A readiness, with the slot it was holding either kept or already given back.
#[derive(Debug)]
pub enum Slotted<T, P> {
    /// A datagram, and the slot to put it in.
    Received(T, P),
    /// A queued kernel error, and the slot the error path will use.
    Errored(P),
    /// Stale readiness. The slot is gone - this variant does not carry one, which is what makes "released
    /// before the wait" a fact about the type rather than a line someone remembered to write.
    Stale,
    /// The daemon's own I/O failed. The slot is gone with it.
    Failed(std::io::Error),
}

/// Routes the held slot by what the readiness turned out to be, releasing it where it is not needed.
pub fn with_slot<T, P>(readiness: Readiness<T>, permit: P) -> Slotted<T, P> {
    match readiness {
        Readiness::Received(received) => Slotted::Received(received, permit),
        Readiness::Errored => Slotted::Errored(permit),
        // Dropped by not being carried.
        Readiness::Stale => Slotted::Stale,
        Readiness::Failed(e) => Slotted::Failed(e),
    }
}

/// Where one error-readiness turn gets its messages.
pub trait ErrorSource {
    /// Takes the next message, or answers [Drained::Empty] when the queue is dry.
    fn next(&mut self) -> std::io::Result<Drained>;
}

/// What one error-queue read did: which message it saw, and what became of the slot it was holding.
#[derive(Debug)]
pub enum Taken {
    Took { slot: Disposition },
    Failed(std::io::Error),
}

/// Takes exactly one message off the error queue and says what to do with the slot already held.
pub fn take_one<S: ErrorSource + ?Sized>(source: &mut S) -> Taken {
    match source.next() {
        Ok(drained) => Taken::Took {
            slot: disposition(drained),
        },
        Err(e) => Taken::Failed(e),
    }
}

/// The whole of what one error-readiness turn may do.
pub fn disposition(drained: Drained) -> Disposition {
    match drained {
        // The owner is the only thing that knows which client this socket serves, so this is the one kind
        // worth a slot.
        Drained::Remote => Disposition::Send,
        // A local refusal belongs to the send that provoked it, which the owner's send path reads for itself;
        // this worker saw no send and has nothing to attribute it to. A message naming neither is nothing at
        // all. Both give the slot back rather than holding it while looking for something better.
        Drained::Local | Drained::Neither | Drained::Empty => Disposition::Release,
    }
}

/// What one whole reply-worker turn came to.
#[derive(Debug)]
pub enum Turned {
    /// A datagram went through the slot this turn took.
    Sent,
    /// One router error went through it.
    Reported,
    /// The slot went back: stale readiness, an empty or unreportable error queue, or this worker's own end.
    Released,
    /// Cancelled while waiting for a slot. Nothing was read and nothing was allocated.
    Cancelled,
    /// The owner is gone, so there is nobody left to hand a reply to.
    Closed,
    /// The daemon's own I/O failed, which ends this worker.
    Failed(std::io::Error),
}

/// Everything one reply worker needs to take a turn, held together so the turn owns the whole order rather
/// than being handed a step someone else already took.
pub struct Turn<'a, X: std::os::fd::AsRawFd, S: ?Sized, E> {
    pub sender: &'a tokio::sync::mpsc::Sender<E>,
    pub cancel: &'a tokio_util::sync::CancellationToken,
    pub fd: &'a tokio::io::unix::AsyncFd<X>,
    pub interest: tokio::io::Interest,
    /// Borrowed rather than built, so a turn can never leave a worker owning a second ancillary buffer.
    pub errors: &'a mut S,
}

impl<X, S, E> Turn<'_, X, S, E>
where
    X: std::os::fd::AsRawFd,
    S: ErrorSource + ?Sized,
{
    /// One whole turn: wait for readiness, take a slot, read, then spend the slot or give it back.
    pub async fn run<T>(
        self,
        read: impl FnOnce(&X) -> std::io::Result<T>,
        kernel_error: impl Fn(&std::io::Error) -> bool,
        datagram: impl FnOnce(T) -> E,
        reported: impl FnOnce(&mut S) -> Option<E>,
    ) -> Turned {
        let Turn {
            sender,
            cancel,
            fd,
            interest,
            errors,
        } = self;
        // Readiness first: free, and what says there is anything worth a slot.
        let mut guard = tokio::select! {
            biased;
            () = cancel.cancelled() => return Turned::Cancelled,
            ready = fd.ready(interest) => match ready {
                Ok(guard) => guard,
                Err(error) => return Turned::Failed(error),
            },
        };
        // Then the slot, and cancellation races it: a retirement must not wait on a queue the owner has
        // stopped draining in order to retire.
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Turned::Cancelled,
            permit = sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => return Turned::Closed,
            },
        };
        // Only now is anything read or allocated.
        let attempt = guard.try_io(|inner| read(inner.get_ref()));
        match with_slot(classify_readiness(attempt, kernel_error), permit) {
            Slotted::Received(payload, permit) => {
                permit.send(datagram(payload));
                Turned::Sent
            }
            // The slot went back with the routing above.
            Slotted::Stale => Turned::Released,
            Slotted::Failed(e) => Turned::Failed(e),
            Slotted::Errored(permit) => match take_one(errors) {
                Taken::Failed(e) => {
                    drop(permit);
                    Turned::Failed(e)
                }
                // Exactly one message, whatever kind. The socket stays error-readable while any remain, so
                // the next turn takes a fresh slot for the next one - with a scheduling boundary, and a
                // cancellation check, in between.
                Taken::Took {
                    slot: Disposition::Send,
                    ..
                } => match reported(errors) {
                    Some(event) => {
                        permit.send(event);
                        Turned::Reported
                    }
                    None => {
                        drop(permit);
                        Turned::Released
                    }
                },
                Taken::Took {
                    slot: Disposition::Release,
                    ..
                } => {
                    drop(permit);
                    Turned::Released
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll_once<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) -> Option<F::Output> {
        use std::task::{Context, Poll, Waker};
        let waker = Waker::noop();
        match future.as_mut().poll(&mut Context::from_waker(waker)) {
            Poll::Ready(value) => Some(value),
            Poll::Pending => None,
        }
    }

    mod pinned {
        pub(super) const BLOCK_CAP_64: u64 = 32;
        pub(super) const BLOCK_CAP_32: u64 = 16;
        pub(super) const SPARE: u64 = 3;

        pub(super) fn retained(
            depth: u64,
            producers: u64,
            value_bytes: u64,
            align: u64,
            block_cap: u64,
            word: u64,
        ) -> u64 {
            let blocks = depth.div_ceil(block_cap) + producers.min(depth) + SPARE;
            blocks * (block_cap * value_bytes + 4 * word + align)
        }
    }

    #[test]
    fn the_charge_covers_the_pinned_mpsc_layout_under_contention() {
        #[allow(dead_code)]
        struct Large([u8; 4096]);
        #[allow(dead_code)]
        #[repr(align(64))]
        struct Aligned(u8);
        struct Empty;

        fn check<T>(label: &str) {
            let value_bytes = std::mem::size_of::<T>() as u64;
            let align = std::mem::align_of::<T>() as u64;
            for depth in [1usize, 2, 16, 17, 32, 33, 500] {
                for producers in [1usize, 2, 32, 500, 4096] {
                    let charged = channel_footprint::<T>(depth, producers).expect("chargeable");
                    for (target, block_cap, word) in [
                        ("64-bit", pinned::BLOCK_CAP_64, 8u64),
                        ("32-bit", pinned::BLOCK_CAP_32, 4u64),
                    ] {
                        let retained = pinned::retained(
                            depth as u64,
                            producers as u64,
                            value_bytes,
                            align,
                            block_cap,
                            word,
                        );
                        assert!(
                            charged >= retained,
                            "{label} depth {depth} producers {producers} on {target}: \
                             {charged} charged is short of the {retained} retained"
                        );
                    }
                }
            }
        }

        check::<Large>("large");
        check::<Aligned>("over-aligned");
        check::<Empty>("zero-sized");
        check::<u8>("byte");
    }

    #[test]
    fn a_depth_one_channel_is_charged_for_the_blocks_it_recycles() {
        #[allow(dead_code)]
        struct Large([u8; 4096]);

        let charged = channel_footprint::<Large>(built_depth(0), 1).expect("chargeable");
        let value_bytes = std::mem::size_of::<Large>() as u64;
        let blocks = 1 + 1 + pinned::SPARE;
        assert!(
            charged >= blocks * pinned::BLOCK_CAP_64 * value_bytes,
            "{charged} charged does not cover {blocks} blocks of {} values",
            pinned::BLOCK_CAP_64
        );
        assert!(
            charged >= 2 * pinned::BLOCK_CAP_64 * value_bytes,
            "{charged} charged does not cover the two blocks a cycled depth-one channel holds"
        );
    }

    #[test]
    fn a_contended_channel_crosses_block_boundaries_with_every_sender_at_once() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .build()
            .expect("a runtime");
        runtime.block_on(async {
            let producers = 32usize;
            let per_producer = 64usize;
            let (sender, mut receiver) = tokio::sync::mpsc::channel::<usize>(producers);
            let senders: Vec<_> = (0..producers).map(|_| sender.clone()).collect();
            drop(sender);
            let mut tasks = tokio::task::JoinSet::new();
            for (id, sender) in senders.into_iter().enumerate() {
                tasks.spawn(async move {
                    for message in 0..per_producer {
                        sender
                            .send(id * per_producer + message)
                            .await
                            .expect("the receiver is draining");
                    }
                });
            }
            let mut seen = 0usize;
            while let Some(_message) = receiver.recv().await {
                seen += 1;
            }
            while let Some(joined) = tasks.join_next().await {
                joined.expect("no producer panicked");
            }
            assert_eq!(
                seen,
                producers * per_producer,
                "every message from every contending sender arrived"
            );
        });
    }

    #[test]
    fn a_channel_charge_covers_more_than_its_payload_slots() {
        #[allow(dead_code)]
        struct Value([u8; 16]);

        for depth in [1usize, 15, 16, 17, 32, 500] {
            let slots = depth as u64 * std::mem::size_of::<Value>() as u64;
            let charged = channel_footprint::<Value>(depth, 1).expect("chargeable");
            assert!(
                charged > slots + CHANNEL_STATE_BYTES,
                "depth {depth}: {charged} does not cover {slots} of values plus the shared state"
            );
            let blocks = blocks_for(depth as u64, 1).expect("fits");
            assert!(blocks >= depth as u64 / VALUES_PER_BLOCK + 2);
            assert!(charged >= blocks * BLOCK_HEADER_BYTES);
        }

        let mut previous = 0;
        for depth in 1..200usize {
            let charged = channel_footprint::<Value>(depth, 1).expect("chargeable");
            assert!(
                charged >= previous,
                "depth {depth} charged less than {depth} - 1"
            );
            previous = charged;
        }
    }

    #[test]
    fn a_reply_channel_charge_covers_the_channel_and_its_payloads() {
        #[allow(dead_code)]
        struct Value([u8; 16]);
        let depth = 32usize;
        let payload = 65_535u64;
        let charged = reply_channel_footprint::<Value>(depth, 1, payload).expect("chargeable");
        let channel = channel_footprint::<Value>(depth, 1).expect("chargeable");
        assert_eq!(charged, channel + (depth as u64 + 1) * payload);
        assert!(
            charged > depth as u64 * payload,
            "the channel itself counts"
        );
        assert!(charged > channel);
    }

    #[test]
    fn the_payload_term_covers_a_full_queue_plus_the_one_in_hand() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .build()
            .expect("a runtime");
        runtime.block_on(async {
            const DEPTH: usize = 4;
            const PAYLOAD: usize = 1 << 10;
            let (sender, mut receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(DEPTH);
            for _ in 0..DEPTH {
                sender
                    .send(vec![0u8; PAYLOAD])
                    .await
                    .expect("the queue has room");
            }
            let in_hand = receiver.recv().await.expect("a queued payload");
            sender
                .send(vec![0u8; PAYLOAD])
                .await
                .expect("the permit came back when the message was taken");
            assert_eq!(
                sender.capacity(),
                0,
                "the queue is full again while one payload is still in hand"
            );
            let live = DEPTH + 1;
            assert_eq!(in_hand.len(), PAYLOAD);

            let charged = reply_channel_footprint::<Vec<u8>>(DEPTH, DEPTH, PAYLOAD as u64)
                .expect("chargeable");
            let channel = channel_footprint::<Vec<u8>>(DEPTH, DEPTH).expect("chargeable");
            assert!(
                charged - channel >= live as u64 * PAYLOAD as u64,
                "{} charged for payloads does not cover the {live} really alive",
                charged - channel
            );
            drop(in_hand);
        });
    }

    #[test]
    fn a_charge_that_would_wrap_fails_closed() {
        #[allow(dead_code)]
        struct Huge([u8; 4096]);
        assert_eq!(channel_footprint::<Huge>(usize::MAX, 1), None);
        assert_eq!(reply_channel_footprint::<Huge>(usize::MAX, 1, 1), None);
        assert_eq!(reply_channel_footprint::<u8>(usize::MAX, 1, u64::MAX), None);
        assert_eq!(blocks_for(u64::MAX, 1), None);
    }

    struct Scripted {
        backlog: std::collections::VecDeque<Drained>,
        asked: usize,
        built: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Scripted {
        fn new(backlog: Vec<Drained>, built: &std::rc::Rc<std::cell::Cell<usize>>) -> Self {
            built.set(built.get() + 1);
            Self {
                backlog: backlog.into(),
                asked: 0,
                built: std::rc::Rc::clone(built),
            }
        }
    }

    impl Drop for Scripted {
        fn drop(&mut self) {
            self.built.set(self.built.get() - 1);
        }
    }

    impl ErrorSource for Scripted {
        fn next(&mut self) -> std::io::Result<Drained> {
            self.asked += 1;
            Ok(self.backlog.pop_front().unwrap_or(Drained::Empty))
        }
    }

    struct Pipe {
        read: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
        write: std::os::fd::OwnedFd,
    }

    impl Pipe {
        fn new() -> Self {
            use std::os::fd::{FromRawFd, OwnedFd};
            let mut ends = [0 as libc::c_int; 2];
            let made =
                unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
            assert_eq!(made, 0, "{}", std::io::Error::last_os_error());
            let (read, write) =
                unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
            Self {
                read: tokio::io::unix::AsyncFd::new(read).expect("a nonblocking pipe registers"),
                write,
            }
        }

        fn feed(&self, byte: u8) {
            use std::os::fd::AsRawFd;
            let wrote = unsafe { libc::write(self.write.as_raw_fd(), [byte].as_ptr().cast(), 1) };
            assert_eq!(wrote, 1, "{}", std::io::Error::last_os_error());
        }
    }

    fn read_one(
        allocations: &std::cell::Cell<usize>,
    ) -> impl FnOnce(&std::os::fd::OwnedFd) -> std::io::Result<Vec<u8>> + '_ {
        move |inner| {
            use std::os::fd::AsRawFd;
            let mut byte = [0u8; 1];
            let read =
                unsafe { libc::read(inner.as_raw_fd(), byte.as_mut_ptr().cast(), byte.len()) };
            if read < 0 {
                return Err(std::io::Error::last_os_error());
            }
            allocations.set(allocations.get() + 1);
            Ok(byte[..read as usize].to_vec())
        }
    }

    #[tokio::test]
    async fn the_production_turn_orders_readiness_slot_and_allocation() {
        let pipe = Pipe::new();
        let built = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
        let cancel = tokio_util::sync::CancellationToken::new();
        let kernel = |e: &std::io::Error| e.kind() == std::io::ErrorKind::ConnectionRefused;
        let allocations = std::cell::Cell::new(0usize);
        let mut source = Scripted::new(Vec::new(), &built);

        pipe.feed(7);
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(read_one(&allocations), kernel, |payload| payload, |_| None)
        .await;
        assert!(matches!(turned, Turned::Sent), "{turned:?}");
        assert_eq!(allocations.get(), 1);
        assert_eq!(receiver.recv().await, Some(vec![7]));

        pipe.feed(9);
        let drained = {
            use std::os::fd::AsRawFd;
            let mut byte = [0u8; 1];
            unsafe { libc::read(pipe.read.get_ref().as_raw_fd(), byte.as_mut_ptr().cast(), 1) }
        };
        assert_eq!(
            drained, 1,
            "consumed behind the turn's back, as another reader would"
        );
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(read_one(&allocations), kernel, |payload| payload, |_| None)
        .await;
        assert!(
            matches!(turned, Turned::Released),
            "a real WouldBlock is stale, not a failure: {turned:?}"
        );
        assert_eq!(allocations.get(), 1);
        assert_eq!(sender.capacity(), 2);
        assert_eq!(source.asked, 0);

        pipe.feed(11);
        let held = [
            sender.reserve().await.expect("a slot"),
            sender.reserve().await.expect("a slot"),
        ];
        {
            let turn = Turn {
                sender: &sender,
                cancel: &cancel,
                fd: &pipe.read,
                interest: tokio::io::Interest::READABLE,
                errors: &mut source,
            }
            .run(read_one(&allocations), kernel, |payload| payload, |_| None);
            tokio::pin!(turn);
            assert!(
                poll_once(turn.as_mut()).is_none(),
                "a full channel parks it"
            );
            assert_eq!(allocations.get(), 1);
        }
        assert_eq!(source.asked, 0);
        for permit in held {
            drop(permit);
        }
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(read_one(&allocations), kernel, |payload| payload, |_| None)
        .await;
        assert!(matches!(turned, Turned::Sent), "{turned:?}");
        assert_eq!(allocations.get(), 2);
        assert_eq!(receiver.recv().await, Some(vec![11]));
        assert_eq!(built.get(), 1);
    }

    #[tokio::test]
    async fn the_production_turn_spends_one_slot_on_one_error() {
        let pipe = Pipe::new();
        let built = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<&'static str>(2);
        let cancel = tokio_util::sync::CancellationToken::new();
        let kernel = |e: &std::io::Error| e.kind() == std::io::ErrorKind::ConnectionRefused;
        let errored = |_: &std::os::fd::OwnedFd| -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
        };

        let mut backlog = vec![Drained::Local; 5_000];
        backlog.extend(vec![Drained::Neither; 5_000]);
        backlog.push(Drained::Remote);
        let mut source = Scripted::new(backlog, &built);

        for turn in 0..10_000 {
            pipe.feed(1);
            let asked = source.asked;
            let turned = Turn {
                sender: &sender,
                cancel: &cancel,
                fd: &pipe.read,
                interest: tokio::io::Interest::READABLE,
                errors: &mut source,
            }
            .run(errored, kernel, |_| "payload", |_| Some("reported"))
            .await;
            assert!(
                matches!(turned, Turned::Released),
                "turn {turn}: {turned:?}"
            );
            assert_eq!(source.asked, asked + 1, "turn {turn}: one message only");
            assert_eq!(sender.capacity(), 2, "turn {turn}: its slot went back");
            assert_eq!(built.get(), 1, "turn {turn}: one scratch");
        }

        pipe.feed(1);
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(errored, kernel, |_| "payload", |_| Some("reported"))
        .await;
        assert!(matches!(turned, Turned::Reported), "{turned:?}");
        assert_eq!(receiver.recv().await, Some("reported"));

        cancel.cancel();
        pipe.feed(1);
        let asked = source.asked;
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(errored, kernel, |_| "payload", |_| Some("reported"))
        .await;
        assert!(matches!(turned, Turned::Cancelled), "{turned:?}");
        assert_eq!(source.asked, asked);
        assert_eq!(built.get(), 1);
    }

    #[test]
    fn a_zero_capacity_request_still_builds_and_charges_a_channel() {
        assert_eq!(
            built_depth(0),
            1,
            "a zero-capacity channel is not constructible"
        );
        assert_eq!(built_depth(1), 1);
        assert_eq!(built_depth(7), 7);
        let zero = channel_footprint::<u64>(built_depth(0), 1).expect("chargeable");
        assert!(zero > 0);
        assert_eq!(zero, channel_footprint::<u64>(1, 1).expect("chargeable"));
    }

    #[test]
    fn one_turn_disposes_of_exactly_one_message() {
        assert_eq!(disposition(Drained::Remote), Disposition::Send);
        assert_eq!(disposition(Drained::Local), Disposition::Release);
        assert_eq!(disposition(Drained::Neither), Disposition::Release);
        assert_eq!(disposition(Drained::Empty), Disposition::Release);
    }

    #[test]
    fn an_unreportable_backlog_never_holds_a_slot() {
        let backlog = [Drained::Local; 10_000];
        let spent = backlog
            .iter()
            .filter(|drained| disposition(**drained) == Disposition::Send)
            .count();
        assert_eq!(spent, 0);
        for drained in backlog {
            assert!(matches!(
                disposition(drained),
                Disposition::Send | Disposition::Release
            ));
        }
    }
}
