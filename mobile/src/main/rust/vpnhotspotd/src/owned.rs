//! What the delivery path really owns, at the moments it owns it.
//!
//! The accounting says a delivery may hold the answer, one framed copy of it and one chunk on its way to the
//! client's stack. That is a statement about buffers this process has allocated and not yet dropped, and the
//! only place it can be checked is where those allocations happen - so each of them takes a [Held] for as
//! long as it exists, and a test reads the peak afterwards.
//!
//! The peak answers *how many* existed at once, which is what catches a replacement built beside the buffer it
//! replaces. What it cannot answer on its own is the *order* two buffers came and went in - an owner that
//! builds a replacement and then drops the obsolete original ends where one that drops first ends, and only
//! the peak between them differs. So each [Held] is also dated, on this module's own monotone counter of
//! buffer events, which makes "the obsolete one was gone before the replacement existed" a question with an
//! answer rather than a comment.
//!
//! In a build that is not a test harness this is nothing: [Held] is a zero-sized value, [hold] does no work,
//! and the drop is empty. Nothing here reaches into the admission ledger, in either direction - the
//! *capacity* side of the same property belongs to [vpnhotspotd::shared::dns_debt], which owns both the grant
//! and the value it covers and is checkable beside them. The calls stay in the daemon's own code rather than
//! being a shape a test rebuilds, because what they prove is a property of *that* code - an implementation
//! that framed the whole response into pieces up front would satisfy every queue depth there is and hold a
//! second whole copy while doing it, and the only thing that catches it is counting at the allocation.
//!
//! # Why a thread-local rather than a process-wide counter
//!
//! Tests share a process. The producer here is a spawned task, and a `#[tokio::test]` runs its tasks on the
//! thread that drives them, so the producer's allocations and the test's assertions are on one thread and
//! two tests running at once cannot see each other's.

/// One owned buffer, counted for as long as this value lives.
///
/// Held beside the allocation rather than inside it: a piece handed to the mailbox is dropped by whoever
/// consumes it, and what the depth-one contract actually says is that the producer does not build the next
/// one until this one has been *acknowledged* - so the producer holding this across the handover is exactly
/// the buffer's life as the contract means it.
#[must_use]
pub(crate) struct Held {
    #[cfg(test)]
    bytes: usize,
    /// Where this buffer's life is recorded, so its drop can close the entry its birth opened.
    #[cfg(test)]
    life: usize,
}

/// Counts one buffer of `bytes` until the returned value is dropped.
pub(crate) fn hold(bytes: usize) -> Held {
    #[cfg(not(test))]
    let _ = bytes;
    Held {
        #[cfg(test)]
        bytes,
        #[cfg(test)]
        life: counted::take(bytes),
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        #[cfg(test)]
        counted::give_back(self.bytes, self.life);
    }
}

/// One owned byte buffer, counted from the moment this daemon takes it to the moment it is dropped.
///
/// The guard is *inside*, which is the whole point: the buffer moves through channels, records and owners,
/// and an accounting that lived beside it at any one of those would stop counting the moment it moved. What
/// drops the buffer drops the count with it, wherever that happens.
pub(crate) struct Owned {
    bytes: Vec<u8>,
    _held: Held,
}

impl Owned {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self {
            _held: hold(bytes.capacity()),
            bytes,
        }
    }

    /// An empty buffer at a capacity its owner has already been granted.
    ///
    /// The counterpart of [Owned::new] for a buffer that is filled after it is admitted rather than copied
    /// out of something that already exists - a DNS-over-TCP query, which is accumulated off the client's
    /// stream only once the owner has reserved the exchange it belongs to. Counted at its capacity from here,
    /// because that is what the reservation covers and what this process really holds.
    pub(crate) fn with_capacity(bytes: usize) -> Self {
        Self::new(Vec::with_capacity(bytes))
    }

    /// Appends as much of `bytes` as the granted capacity still has room for, and answers how much that was.
    ///
    /// Never reallocates, which is the point: growing would be an allocation past what was charged, and the
    /// caller has already been told how large this buffer may be. A short answer means the peer sent more
    /// than the length it announced, which its caller treats as the framing error it is.
    pub(crate) fn extend_within_capacity(&mut self, bytes: &[u8]) -> usize {
        let room = self.bytes.capacity() - self.bytes.len();
        let taken = room.min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..taken]);
        taken
    }

    /// What this buffer really cost, which is what it was counted at rather than what it currently holds.
    /// An owner reconciling a conservative reservation downward has to read the same figure the count was
    /// taken from, or the two would disagree about the same buffer.
    pub(crate) fn capacity(&self) -> usize {
        self.bytes.capacity()
    }
}

/// Where a framed DNS message's bytes go. The contract is [Owned::extend_within_capacity]'s: a buffer that
/// was charged before it existed may be filled and may never grow.
impl vpnhotspotd::shared::dns_wire::Body for Owned {
    fn extend_within_capacity(&mut self, bytes: &[u8]) -> usize {
        Owned::extend_within_capacity(self, bytes)
    }
}

impl std::ops::Deref for Owned {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl std::fmt::Debug for Owned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes", self.bytes.len())
    }
}

#[cfg(test)]
pub(crate) use counted::{lives, mark, peak, reset, Peak};

#[cfg(test)]
mod counted {
    use std::cell::{Cell, RefCell};

    /// The most this thread ever owned at once, which is what a bound is about - an end-state check passes
    /// for an implementation that allocated everything and then freed it.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Peak {
        pub(crate) buffers: usize,
        pub(crate) bytes: usize,
    }

    /// One buffer's whole life, dated on this module's own counter of buffer events.
    ///
    /// A balance says nothing about order, and neither does a peak on its own: what tells a replacement built
    /// beside its predecessor from one built after it is asking each buffer when it came into existence and
    /// when it went. Neither number is written by the code under test - both are taken where the allocation
    /// itself happens.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct Life {
        pub(crate) bytes: usize,
        /// Which buffer event this buffer's birth was.
        pub(crate) born: u64,
        /// ...and its death. `None` while it is still alive.
        pub(crate) died: Option<u64>,
    }

    thread_local! {
        static LIVE: Cell<Peak> = const { Cell::new(Peak { buffers: 0, bytes: 0 }) };
        static PEAK: Cell<Peak> = const { Cell::new(Peak { buffers: 0, bytes: 0 }) };
        static LIVES: RefCell<Vec<Life>> = const { RefCell::new(Vec::new()) };
        static CLOCK: Cell<u64> = const { Cell::new(0) };
    }

    /// The next buffer event on this thread. Monotone, so any two buffers can be ordered against each other.
    fn tick() -> u64 {
        CLOCK.with(|clock| {
            let now = clock.get();
            clock.set(now + 1);
            now
        })
    }

    pub(crate) fn take(bytes: usize) -> usize {
        let live = LIVE.with(|live| {
            let mut now = live.get();
            now.buffers += 1;
            now.bytes += bytes;
            live.set(now);
            now
        });
        PEAK.with(|peak| {
            let mut high = peak.get();
            high.buffers = high.buffers.max(live.buffers);
            high.bytes = high.bytes.max(live.bytes);
            peak.set(high);
        });
        let born = tick();
        LIVES.with(|lives| {
            let mut lives = lives.borrow_mut();
            lives.push(Life {
                bytes,
                born,
                died: None,
            });
            lives.len() - 1
        })
    }

    pub(crate) fn give_back(bytes: usize, life: usize) {
        LIVE.with(|live| {
            let mut now = live.get();
            now.buffers -= 1;
            now.bytes -= bytes;
            live.set(now);
        });
        let died = tick();
        // A buffer that outlived a reset has no entry any more, which is a test that cleared the log while
        // something was still alive rather than anything the daemon did.
        LIVES.with(|lives| {
            if let Some(entry) = lives.borrow_mut().get_mut(life) {
                entry.died = Some(died);
            }
        });
    }

    /// Dates one moment that is not a buffer's birth or death, on the same clock.
    ///
    /// What it exists for is an owner whose ordering claim is about a buffer and something that is *not* a
    /// buffer - a grant becoming releasable, say. Ordering those two against each other needs one clock, and
    /// this is the clock the buffers are already on. Nothing outside a test harness calls it.
    pub(crate) fn mark() -> u64 {
        tick()
    }

    /// What is owned now, and the most that ever was.
    pub(crate) fn peak() -> (Peak, Peak) {
        (LIVE.with(Cell::get), PEAK.with(Cell::get))
    }

    /// Every buffer this thread has owned since the last reset, in the order they were taken.
    pub(crate) fn lives() -> Vec<Life> {
        LIVES.with(|lives| lives.borrow().clone())
    }

    pub(crate) fn reset() {
        LIVE.with(|live| live.set(Peak::default()));
        PEAK.with(|peak| peak.set(Peak::default()));
        LIVES.with(|lives| lives.borrow_mut().clear());
        CLOCK.with(|clock| clock.set(0));
    }
}
