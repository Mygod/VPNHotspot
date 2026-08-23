//! What a bounded reply channel really costs, and what one error-queue turn is allowed to do.
//!
//! Both live here rather than beside the sockets they serve because both are decisions rather than I/O, and a
//! decision that cannot be tested is one nobody can check. The daemon's own reply paths are wrapped around
//! platform sockets and a Tokio runtime; the two things that actually bound them are arithmetic and a
//! four-way match, and those are here.
//!
//! # Charging a channel, not a message
//!
//! A `depth`-slot channel is not `depth * size_of::<T>()` of memory. Tokio's bounded MPSC keeps its state in
//! a reference-counted allocation, and its values in a linked list of fixed-size *blocks* that are allocated
//! as the queue fills and kept for reuse. Charging only the payload slots understates every one of those, and
//! an understated bound is the fail-open case the aggregate exists to prevent.
//!
//! What is charged here is deliberately an over-estimate of that layout rather than a reproduction of it, and
//! the scope of the claim is exactly the runtime this crate locks. Tokio's block size, header shape and
//! recycling policy are `pub(crate)`, so this is **not** a cross-version theorem: the constants below were
//! read off the locked `tokio 1.53.1` for the 64-bit and 32-bit targets this daemon is built for, and each is
//! an upper bound over *both* so a target difference cannot make them short. Bumping the runtime dependency
//! means re-reading `sync/mpsc/mod.rs`, `sync/mpsc/block.rs` and `sync/mpsc/list.rs` and revalidating them.
//!
//! The audited facts, for whoever has to do that:
//!
//! - `BLOCK_CAP` is 32 where a `usize` is 64 bits and 16 where it is 32 - `sync/mpsc/mod.rs:138-143`.
//! - one `Block<T>` is a four-word `BlockHeader<T>` - start index, next pointer, ready bitfield, observed
//!   tail position - followed by `BLOCK_CAP` inline value slots: `sync/mpsc/block.rs:14-47`.
//! - a drained block is *not* freed. `reclaim_block` resets it and tries to re-append it at the tail,
//!   walking at most three blocks, and drops it only if all three attempts fail -
//!   `sync/mpsc/list.rs:194-241`. So a channel cycled at a shallow depth keeps blocks in hand rather than
//!   returning them, and the bound has to allow for that rather than for `depth` alone.

use std::marker::PhantomData;

use crate::shared::admission::linear_footprint;

/// The value slots one charged block covers: the *largest* `BLOCK_CAP` across the pinned targets.
///
/// The largest rather than the smallest, and that is the correction. A block really does hold 32 values on
/// every 64-bit target, so charging 16 makes the *block itself* half the size it is - and no number of extra
/// block headers makes up for value slots that were never counted. [linear_footprint] doubles this again, so
/// one charged block covers 64 slots against a real 32.
const CHARGED_BLOCK_VALUES: u64 = 32;

/// The divisor used to decide how many blocks a depth needs: the *smallest* `BLOCK_CAP` across the pinned
/// targets.
///
/// The smallest here, for the reason the largest is used above: fewer values per block means more blocks for a
/// given depth. Taking the extremes on the two axes separately is what makes the product an upper bound on
/// both targets at once rather than on whichever one it was written against.
const VALUES_PER_BLOCK: u64 = 16;

/// Blocks the *recycling* may hold beyond the ones a channel's depth needs.
///
/// Three, and each one is a place the audited source really puts a block: the one the receiver has drained
/// but not yet handed back, the one `reclaim_block` re-appended at the tail for reuse, and the one that
/// function owns outright between resetting it and re-appending or dropping it. It is a bound on *this*
/// runtime's recycling rather than a general claim - see the module note.
const SPARE_BLOCKS: u64 = 3;

/// An upper bound on one block's own overhead beyond the values in it: its link pointer, its start index, and
/// the atomic ready/observed bitfields it keeps per slot.
///
/// Four machine words of header plus two bitfield words, doubled. Doubling is what covers a layout change
/// that adds a field rather than requiring this to track one.
const BLOCK_HEADER_BYTES: u64 = (std::mem::size_of::<usize>() as u64) * 6 * 2;

/// An upper bound on the shared channel state: the reference-counted `Chan`, its semaphore, the two waker
/// slots, the receiver's cursor and the sender/receiver handles the owner retains.
///
/// A kilobyte. That is far more than the structure needs on any target here, and it is chosen to be far more
/// on purpose: this is charged once per channel, so a generous figure costs a few kilobytes across the whole
/// dataplane and buys a bound that does not have to be revisited when a runtime version reorganises itself.
const CHANNEL_STATE_BYTES: u64 = 1024;

/// How many blocks a channel of `depth` values with `producers` senders may hold at once.
///
/// Three terms, and the middle one is the correction a multi-producer channel needs:
///
/// - one block per block's worth of depth, rounded up, for the values themselves;
/// - one block per sender that can *lose* a grow race. `Block::grow` allocates its block before the
///   compare-exchange and deliberately does not drop it when it loses - it walks the list and appends it at
///   the next empty link (`sync/mpsc/block.rs:357-415`), so a losing racer's block joins the list rather than
///   going away. At a block boundary every sender inside `send` can be in that race at once;
/// - and [SPARE_BLOCKS] for the recycling.
///
/// The racer term is capped at `depth` because a sender is only inside `send` while it holds one of the
/// channel's permits, and there are exactly `depth` of those. That cap is what keeps this a bound rather than
/// a function of however many sender handles exist.
fn blocks_for(depth: u64, producers: u64) -> Option<u64> {
    depth
        .checked_add(VALUES_PER_BLOCK - 1)?
        .checked_div(VALUES_PER_BLOCK)?
        .checked_add(producers.min(depth))?
        .checked_add(SPARE_BLOCKS)
}

/// A conservative upper bound on everything a bounded channel of `depth` values of `T` retains, for an owner
/// that may have `producers` senders inside `send` at once.
///
/// `producers` is a dimension this daemon really chooses rather than one read out of the runtime: a channel
/// whose sender it never clones has exactly one, and a fan-in queue has one per worker it prepared. It is
/// what the grow-race term in [blocks_for] is for, so an owner that clones its sender has to say so.
///
/// Conservative and checked: an owner charges this once, before the channel exists, and a wrapping figure is
/// a channel that cannot be accounted for and therefore must not be created.
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
///
/// The payloads belong here rather than to whoever puts one in, and that is the ordering the reply path
/// depends on: a worker takes a slot *before* it sizes or allocates a datagram, so what can be in flight is a
/// function of the depth rather than of however many workers exist. Charging a maximum payload per worker
/// instead is a figure that grows with the mapping table and bounds nothing.
///
/// **`depth + 1`, not `depth`.** Taking a message off a Tokio channel returns its permit immediately, and the
/// owner then works on that message with the permit already free - the ingress task reads an `Event` and its
/// `Vec` stays alive through `udp::handle` or `echo::handle`. On the multi-thread runtime a producer can
/// refill that freed slot, and every other slot, while the receiver still holds the one it took. So the
/// physical peak is a full queue *plus* the one in the owner's hand. This is the same shape as the
/// client-to-upstream peak in [crate::tcp]'s per-flow footprint, and it was short here for the same reason
/// that one was: a depth-one channel is not a one-buffer bound.
pub fn reply_channel_footprint<T>(depth: usize, producers: usize, max_payload: u64) -> Option<u64> {
    let depth = u64::try_from(depth).ok()?;
    channel_footprint::<T>(depth as usize, producers)?
        .checked_add(depth.checked_add(1)?.checked_mul(max_payload)?)
}

/// The depth a channel is really built at, for a requested capacity.
///
/// At least one, because a zero-capacity channel is not constructible. Stated as a function so that the
/// charge and the construction read the same number: an owner whose derived bound came out zero still
/// allocates this, and a minimum quietly assumed free at construction is a real allocation nobody charged -
/// the fail-open shape the aggregate exists to prevent.
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
///
/// The three are genuinely different and the middle one is the easy mistake: readiness that is stale by the
/// time the socket is read is not a failure and not a datagram, and a worker that kept its slot across the
/// wait for the *next* readiness would hold a slot other mappings' replies need for as long as its own
/// socket stayed quiet.
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
///
/// `Err(_)` from `try_io` is "the readiness was stale", which is what the outer `Result` means; the inner one
/// is the read itself. `kernel_error` decides which failures are a queued ICMP error rather than a fault,
/// because that is a socket question and not this module's.
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
///
/// The release happens *here*, by taking the permit by value and not putting it in the two variants that do
/// not have anything for it. A worker that kept its slot across the wait for the next readiness would hold
/// one that other mappings' replies need, for as long as its own socket stayed quiet - and "remember to drop
/// it on this branch" is exactly the kind of thing that is right until someone adds a branch.
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
///
/// A trait so the turn below can be driven without a socket: production's is the daemon's own error queue,
/// borrowing the scratch its owner holds, and a test's is a scripted backlog. What matters either way is that
/// the turn *borrows* it - it never constructs one, so a worker can never come to own a second.
pub trait ErrorSource {
    /// Takes the next message, or answers [Drained::Empty] when the queue is dry.
    fn next(&mut self) -> std::io::Result<Drained>;
}

/// What one error-queue read did: which message it saw, and what became of the slot it was holding.
#[derive(Debug)]
pub enum Taken {
    Took { drained: Drained, slot: Disposition },
    Failed(std::io::Error),
}

/// Takes exactly one message off the error queue and says what to do with the slot already held.
///
/// One, and the count is the point. Draining until something reportable turned up ran a loop a remote sizes,
/// between two await points, so a retirement asking this worker to stop waited out the whole backlog first.
/// Borrowing the source rather than building one is the other half: a turn that constructed its own scratch
/// would give a worker a second ancillary buffer beside the one it already owns for its life.
pub fn take_one<S: ErrorSource + ?Sized>(source: &mut S) -> Taken {
    match source.next() {
        Ok(drained) => Taken::Took {
            drained,
            slot: disposition(drained),
        },
        Err(e) => Taken::Failed(e),
    }
}

/// The whole of what one error-readiness turn may do.
///
/// One message per turn, whatever kind it is. Draining synchronously until something reportable turned up
/// looked harmless - a local refusal or an unattributable message costs one syscall to skip - but how many of
/// those are queued is a remote's choice, and a loop over them is a loop a sender sizes. Worse, it runs
/// between two `await` points, so a retirement asking this worker to stop waits out the whole backlog. One
/// per turn puts a scheduling boundary between every message: cancellation is checked, other workers run, and
/// the queue still drains at one syscall per turn because each turn removes a message.
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
///
/// The descriptor and the interest are in here for exactly that reason: waiting for readiness *outside* the
/// turn leaves the one ordering decision that matters split across two places, and a caller that reordered it
/// would still compile.
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
    ///
    /// **Readiness first, then the slot.** That order is not interchangeable. A worker that reserved before
    /// waiting would hold one of the queue's few slots for as long as its own socket stayed quiet, and with
    /// far more workers than slots every slot would end up held by an idle one while a socket with a datagram
    /// waited behind them - a livelock built out of correct-looking backpressure. Readiness is free and
    /// allocates nothing; the slot is the scarce thing, so it is taken only once there is something to put in
    /// it.
    ///
    /// **The slot, then the read.** `read` is where the datagram is sized and allocated, and it is only
    /// reachable once a slot has been obtained - so a full queue stops a worker before it allocates rather
    /// than after, and the relay's in-flight cost is the depth that was charged for rather than the number of
    /// mappings.
    ///
    /// **One error message per turn.** The error queue is drained once, not looped over: how many messages are
    /// queued is a remote's choice, and a loop between two await points waits out an attacker-sized backlog
    /// before it notices it has been cancelled.
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

/// A marker for the payload type a footprint was taken for, so a caller cannot charge one channel's shape and
/// build another's.
#[derive(Debug)]
pub struct Charged<T> {
    depth: usize,
    kind: PhantomData<fn() -> T>,
}

impl<T> Charged<T> {
    /// Records that a channel of this depth and payload has been charged for. Built only by an owner that has
    /// a granted lease in hand.
    pub fn new(depth: usize) -> Self {
        Self {
            depth,
            kind: PhantomData,
        }
    }

    pub fn depth(&self) -> usize {
        self.depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Polls a future once and answers whether it was ready. Pending is the assertion, so this must not wait.
    fn poll_once<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) -> Option<F::Output> {
        use std::task::{Context, Poll, Waker};
        // Nothing to wake: the assertion is that this is pending, so a wakeup would have nowhere to go.
        let waker = Waker::noop();
        match future.as_mut().poll(&mut Context::from_waker(waker)) {
            Poll::Ready(value) => Some(value),
            Poll::Pending => None,
        }
    }

    /// The pinned Tokio MPSC layout, restated here as literals so the charge is checked against the runtime
    /// rather than against a second copy of its own formula.
    ///
    /// Read from `tokio 1.53.1`: `BLOCK_CAP` is 32 on a 64-bit target and 16 on a 32-bit one
    /// (`sync/mpsc/mod.rs:138-143`); a block is `BLOCK_CAP` inline value slots behind a four-word header
    /// (`sync/mpsc/block.rs:14-47`); a drained block is re-appended for reuse rather than freed
    /// (`sync/mpsc/list.rs:194-241`); and `Block::grow` allocates before its compare-exchange and, on losing,
    /// walks the list to append its block rather than dropping it (`sync/mpsc/block.rs:357-415`) - so every
    /// sender that can be inside `send` at a block boundary can add a block of its own.
    mod pinned {
        pub(super) const BLOCK_CAP_64: u64 = 32;
        pub(super) const BLOCK_CAP_32: u64 = 16;
        /// Blocks the recycling keeps: drained-not-yet-handed-back, re-appended for reuse, and the one
        /// `reclaim_block` owns while it decides between those two.
        pub(super) const SPARE: u64 = 3;

        /// What one channel really retains on one target: the blocks its values need, one per sender that can
        /// lose a grow race, and the recycling's spares - each at that target's full block width.
        pub(super) fn retained(
            depth: u64,
            producers: u64,
            value_bytes: u64,
            align: u64,
            block_cap: u64,
            word: u64,
        ) -> u64 {
            let blocks = depth.div_ceil(block_cap) + producers.min(depth) + SPARE;
            // Padding between the header and the value array is bounded by the value's alignment.
            blocks * (block_cap * value_bytes + 4 * word + align)
        }
    }

    /// The charge covers what the pinned runtime really retains - on both target widths, under multi-producer
    /// contention, and for message types the arithmetic could have got wrong.
    ///
    /// This is the regression for two real fail-open bugs. The bound used to charge one block as
    /// `BLOCK_CAP = 16` values, which is *half* a real block on every 64-bit target. And it allowed a fixed
    /// three spare blocks, which is not a bound at all for a channel whose sender is cloned: `Block::grow`
    /// does not drop the block a losing racer allocated, it appends it, so at a block boundary every sender
    /// holding a permit can add one.
    ///
    /// The matrix is deliberate. A large `T` so the fixed shared-state cushion cannot paper over a missing
    /// block; an over-aligned `T` so header padding is exercised; and a zero-sized `T`, where the values cost
    /// nothing and the blocks still exist.
    #[test]
    fn the_charge_covers_the_pinned_mpsc_layout_under_contention() {
        // Four kilobytes per value: one missing block is 128 KiB, far past any fixed cushion.
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

    /// A depth-one channel cycled many times keeps whole blocks, and the arithmetic has to cover them.
    ///
    /// The case a depth-scaled spare allowance gets wrong in the direction that matters: the blocks such a
    /// channel owns are not a function of its depth at all.
    #[test]
    fn a_depth_one_channel_is_charged_for_the_blocks_it_recycles() {
        #[allow(dead_code)]
        struct Large([u8; 4096]);

        let charged = channel_footprint::<Large>(built_depth(0), 1).expect("chargeable");
        let value_bytes = std::mem::size_of::<Large>() as u64;
        // One block for its single value, its one possible racer, and the recycling's spares.
        let blocks = 1 + 1 + pinned::SPARE;
        assert!(
            charged >= blocks * pinned::BLOCK_CAP_64 * value_bytes,
            "{charged} charged does not cover {blocks} blocks of {} values",
            pinned::BLOCK_CAP_64
        );
        // And the two-block steady state a cycled depth-one channel really sits in, named on its own so the
        // failure message says which claim broke.
        assert!(
            charged >= 2 * pinned::BLOCK_CAP_64 * value_bytes,
            "{charged} charged does not cover the two blocks a cycled depth-one channel holds"
        );
    }

    /// A real contended channel, driven across many block boundaries by many senders at once.
    ///
    /// What this can and cannot show is worth being exact about. It cannot count Tokio's blocks - the list is
    /// crate-private - so it does not verify the racer term numerically. What it does show is that the
    /// scenario the term exists for is reachable in this runtime rather than theoretical: many cloned senders,
    /// each holding a permit, crossing block boundaries together on a multi-thread runtime, with every message
    /// arriving. The numeric bound is proved against the audited layout above.
    #[test]
    fn a_contended_channel_crosses_block_boundaries_with_every_sender_at_once() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .build()
            .expect("a runtime");
        runtime.block_on(async {
            // Deep enough that every producer can hold a permit at once, and long enough that the tail walks
            // several blocks of either target width.
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

    /// The charge covers the channel's own allocation as well as its payloads, and is an upper bound on the
    /// layout it describes rather than a reproduction of it.
    #[test]
    fn a_channel_charge_covers_more_than_its_payload_slots() {
        // Sixteen-byte values, so the payload slots are easy to name separately.
        #[allow(dead_code)]
        struct Value([u8; 16]);

        for depth in [1usize, 15, 16, 17, 32, 500] {
            let slots = depth as u64 * std::mem::size_of::<Value>() as u64;
            let charged = channel_footprint::<Value>(depth, 1).expect("chargeable");
            assert!(
                charged > slots + CHANNEL_STATE_BYTES,
                "depth {depth}: {charged} does not cover {slots} of values plus the shared state"
            );
            // At least one block per block's worth of depth, and the spares.
            let blocks = blocks_for(depth as u64, 1).expect("fits");
            assert!(blocks >= depth as u64 / VALUES_PER_BLOCK + 2);
            assert!(charged >= blocks * BLOCK_HEADER_BYTES);
        }

        // Monotone in depth, which is what makes it usable as a bound at all.
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

    /// A reply channel's charge is its own allocation plus every payload its slots may hold, and neither half
    /// may be dropped from it.
    #[test]
    fn a_reply_channel_charge_covers_the_channel_and_its_payloads() {
        #[allow(dead_code)]
        struct Value([u8; 16]);
        let depth = 32usize;
        let payload = 65_535u64;
        let charged = reply_channel_footprint::<Value>(depth, 1, payload).expect("chargeable");
        let channel = channel_footprint::<Value>(depth, 1).expect("chargeable");
        // A full queue *plus* the one the receiver has taken and is still working on - taking a message
        // returns its permit, so the slot it came from can be refilled while it is alive.
        assert_eq!(charged, channel + (depth as u64 + 1) * payload);
        assert!(
            charged > depth as u64 * payload,
            "the channel itself counts"
        );
        assert!(charged > channel, "the payloads count");
    }

    /// The payload term really covers a full queue plus the message the owner is holding.
    ///
    /// Driven rather than restated: `depth` producers each send a maximum-sized payload, the receiver takes
    /// one and keeps it, and then the queue is refilled to capacity behind it. What is alive at that moment is
    /// counted from the real allocations, and it is `depth + 1` - which is what the charge has to cover. A
    /// formula charging `depth` fails here by exactly one datagram.
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
            // Fill it.
            for _ in 0..DEPTH {
                sender
                    .send(vec![0u8; PAYLOAD])
                    .await
                    .expect("the queue has room");
            }
            // Take one and keep it, which frees exactly one permit.
            let in_hand = receiver.recv().await.expect("a queued payload");
            // Refill that slot while the taken one is still alive. This is the moment the bound is about.
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
            assert_eq!(in_hand.len(), PAYLOAD, "and the held one is a real payload");

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

    /// Arithmetic that would wrap is a channel that cannot be accounted for, so it is a refusal rather than a
    /// small number.
    #[test]
    fn a_charge_that_would_wrap_fails_closed() {
        #[allow(dead_code)]
        struct Huge([u8; 4096]);
        assert_eq!(channel_footprint::<Huge>(usize::MAX, 1), None);
        assert_eq!(reply_channel_footprint::<Huge>(usize::MAX, 1, 1), None);
        // The payload half wraps on its own too.
        assert_eq!(reply_channel_footprint::<u8>(usize::MAX, 1, u64::MAX), None);
        assert_eq!(blocks_for(u64::MAX, 1), None);
    }

    /// A scripted error queue, standing in for the socket's. It is *borrowed* by every turn, and counts how
    /// many times a turn asked it for a message - which is the whole of what "one per turn" means.
    struct Scripted {
        backlog: std::collections::VecDeque<Drained>,
        asked: usize,
        /// How many of these were ever constructed, so a turn that built its own scratch would be visible.
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

    /// A nonblocking pipe, so readiness and `WouldBlock` are the kernel's rather than a stand-in.
    struct Pipe {
        read: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
        write: std::os::fd::OwnedFd,
    }

    impl Pipe {
        fn new() -> Self {
            use std::os::fd::{FromRawFd, OwnedFd};
            let mut ends = [0 as libc::c_int; 2];
            // SAFETY: pipe2 fills the two descriptors it is given and reads nothing else.
            let made =
                unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
            assert_eq!(made, 0, "{}", std::io::Error::last_os_error());
            // SAFETY: both are freshly created and owned by nothing else.
            let (read, write) =
                unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
            Self {
                read: tokio::io::unix::AsyncFd::new(read).expect("a nonblocking pipe registers"),
                write,
            }
        }

        fn feed(&self, byte: u8) {
            use std::os::fd::AsRawFd;
            // SAFETY: one byte from a local buffer into a descriptor this owns.
            let wrote = unsafe { libc::write(self.write.as_raw_fd(), [byte].as_ptr().cast(), 1) };
            assert_eq!(wrote, 1, "{}", std::io::Error::last_os_error());
        }
    }

    /// Reads one byte, or answers what the kernel said. The production shape: the closure the turn hands to
    /// `try_io`, which is where a datagram would be sized and allocated.
    fn read_one(
        allocations: &std::cell::Cell<usize>,
    ) -> impl FnOnce(&std::os::fd::OwnedFd) -> std::io::Result<Vec<u8>> + '_ {
        move |inner| {
            use std::os::fd::AsRawFd;
            let mut byte = [0u8; 1];
            // SAFETY: one byte into a local buffer from the registered descriptor.
            let read =
                unsafe { libc::read(inner.as_raw_fd(), byte.as_mut_ptr().cast(), byte.len()) };
            if read < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The allocation, and it is only reachable once a slot has been obtained.
            allocations.set(allocations.get() + 1);
            Ok(byte[..read as usize].to_vec())
        }
    }

    /// The whole production turn, driven on a real nonblocking pipe with a real bounded channel: readiness,
    /// then the slot, then the read, then the slot spent or returned.
    ///
    /// Every branch here is the one `reply.rs` runs, and the `WouldBlock` in the stale case is the kernel's -
    /// `try_io` is what turns it into the outer `Err` the stale branch is written for, and no hand-written
    /// `Err(())` can stand in for that.
    #[tokio::test]
    async fn the_production_turn_orders_readiness_slot_and_allocation() {
        let pipe = Pipe::new();
        let built = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
        let cancel = tokio_util::sync::CancellationToken::new();
        let kernel = |e: &std::io::Error| e.kind() == std::io::ErrorKind::ConnectionRefused;
        let allocations = std::cell::Cell::new(0usize);
        let mut source = Scripted::new(Vec::new(), &built);

        // A byte to read: readiness is real, the slot is taken, the read allocates, and that same slot
        // carries it.
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

        // The pipe is empty but a previous readiness is still asserted, so the read really returns EAGAIN and
        // `try_io` really produces the stale branch. The slot goes back, nothing is allocated, and the error
        // queue is never touched.
        pipe.feed(9);
        let drained = {
            use std::os::fd::AsRawFd;
            let mut byte = [0u8; 1];
            // SAFETY: one byte into a local buffer from a descriptor this test owns.
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
        assert_eq!(allocations.get(), 1, "the stale turn allocated nothing");
        assert_eq!(sender.capacity(), 2, "and its slot went back");
        assert_eq!(source.asked, 0, "and it never touched the error queue");

        // A full queue stops the next turn before it reads. Polled rather than awaited: pending is the
        // assertion, and no allocation is the other half of it.
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
            assert_eq!(allocations.get(), 1, "before the read, not after");
        }
        assert_eq!(source.asked, 0, "nor as far as the error queue");
        // A slot back, and the byte still there: the next turn goes through.
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
        assert_eq!(built.get(), 1, "one scratch throughout");
    }

    /// The error branch of the same production turn: one message per turn, and the slot spent only on the
    /// one kind an owner can act on.
    #[tokio::test]
    async fn the_production_turn_spends_one_slot_on_one_error() {
        let pipe = Pipe::new();
        let built = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<&'static str>(2);
        let cancel = tokio_util::sync::CancellationToken::new();
        // Every read reports a queued kernel error, which is what an error-only readiness looks like.
        let kernel = |e: &std::io::Error| e.kind() == std::io::ErrorKind::ConnectionRefused;
        let errored = |_: &std::os::fd::OwnedFd| -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
        };

        // A backlog an attacker could queue, with one real router error at the end.
        let mut backlog = vec![Drained::Local; 5_000];
        backlog.extend(vec![Drained::Neither; 5_000]);
        backlog.push(Drained::Remote);
        let mut source = Scripted::new(backlog, &built);

        // Each unreportable message costs exactly one turn and returns its slot.
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

        // The router error at last: it spends the slot it was given.
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

        // Cancelled with the rest of the backlog still queued: the turn ends at once, having read nothing.
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
        assert_eq!(source.asked, asked, "the backlog was never touched");
        assert_eq!(built.get(), 1, "still exactly one scratch");
    }

    /// A capacity of zero is still a channel, so it is still charged for.
    #[test]
    fn a_zero_capacity_request_still_builds_and_charges_a_channel() {
        assert_eq!(
            built_depth(0),
            1,
            "a zero-capacity channel is not constructible"
        );
        assert_eq!(built_depth(1), 1);
        assert_eq!(built_depth(7), 7);
        // And the minimum is a real allocation with a real charge, not a free one.
        let zero = channel_footprint::<u64>(built_depth(0), 1).expect("chargeable");
        assert!(zero > 0);
        assert_eq!(zero, channel_footprint::<u64>(1, 1).expect("chargeable"));
    }

    /// One drained message per turn, whatever kind it is - and the slot is only spent on the one kind an
    /// owner can do something with.
    #[test]
    fn one_turn_disposes_of_exactly_one_message() {
        assert_eq!(disposition(Drained::Remote), Disposition::Send);
        // The kinds an attacker can queue in bulk give the slot straight back rather than holding it while a
        // synchronous loop looks for something reportable.
        assert_eq!(disposition(Drained::Local), Disposition::Release);
        assert_eq!(disposition(Drained::Neither), Disposition::Release);
        assert_eq!(disposition(Drained::Empty), Disposition::Release);
    }

    /// A backlog of unreportable messages costs one turn each and holds no slot between them, so the worker
    /// reaches a scheduling boundary - and therefore its cancellation - between every one.
    #[test]
    fn an_unreportable_backlog_never_holds_a_slot() {
        // Whatever a sender queues, only the kind an owner can act on ever spends the slot.
        let backlog = [Drained::Local; 10_000];
        let spent = backlog
            .iter()
            .filter(|drained| disposition(**drained) == Disposition::Send)
            .count();
        assert_eq!(spent, 0);
        // And each of them is one turn: there is no disposition that means "keep going without returning".
        for drained in backlog {
            assert!(matches!(
                disposition(drained),
                Disposition::Send | Disposition::Release
            ));
        }
    }
}
