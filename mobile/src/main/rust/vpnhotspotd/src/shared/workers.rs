//! The fence between a worker task's completion and its owner's accounting.
//!
//! Every descriptor the app-UID dataplane opens is serviced by a task of its own, while the table that
//! admitted it lives in the ingress task. That split is what keeps the tables lock-free, and it is also what
//! makes "the descriptor is gone" hard to say honestly. A terminal *message* from a worker proves only that
//! the worker reached its last statement: the socket it read, the `Arc` it shared with its owner and the
//! stream half it wrote are all still alive at that point, so a refund or a config acknowledgement that
//! follows such a message can claim a descriptor is closed while it is open.
//!
//! So here the worker's *completion* is the event. [Workers::finished] yields a terminal only once tokio has
//! dropped that task's future - every socket half, `Arc` and buffer with it - and [Workers::retire] is the
//! only way to get the record back, so the owner's own descriptor owner is dropped after the join rather
//! than before it. Refund, removal and acknowledgement follow that, in that order, and nothing else in the
//! dataplane may reorder them.
//!
//! The same path serves both endings, which is deliberate: a worker that failed on its own and one the owner
//! retired arrive as the same terminal, so there is one place that settles a record rather than two that can
//! disagree about how much it was charged.
//!
//! # Why this is in the library
//!
//! `K` is whatever an owner names a record by - smoltcp's `SocketHandle` for a TCP flow, a client tuple for a
//! UDP mapping - and `R` is the record itself, so nothing here touches a platform API and every owner of the
//! dataplane uses the same copy. It lives here rather than beside one of them because the ordering above is
//! the correctness property and it is silent when it breaks: a table that reported a terminal from a message,
//! or one whose identity check degraded to "this key exists", would keep every caller compiling and every
//! other test green while stranding a descriptor or tearing down a successor. Those are the mutations the
//! tests at the bottom exist to kill, and they can only run against a target the host builds.

use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::hash::Hash;
use std::io;
use std::task::{Context, Poll};

use tokio::task::{Id, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::shared::admission::logical_footprint;
pub use crate::shared::ended::Ended;

/// One finished worker: which record it belonged to, and how it ended.
pub struct Terminal<K> {
    pub key: K,
    /// Which worker under that key. A terminal naming an identity the table has already replaced applies to
    /// nothing, and the owner has to be able to tell.
    pub id: u64,
    pub ended: Ended,
}

/// What a record and its worker share: the identity events are checked against, and the token that stops it.
pub struct Identity {
    pub id: u64,
    pub cancel: CancellationToken,
}

/// One record its owner keeps, beside the identity and token of the worker holding its descriptor.
pub struct Held<R> {
    pub id: u64,
    pub cancel: CancellationToken,
    pub record: R,
}

/// The records one owner admitted, and the tasks that hold their descriptors.
///
/// This exists as a type rather than as a `HashMap` beside a `JoinSet` in each owner because the ordering
/// above is the correctness property, and four owners keeping it by hand is four places for it to drift.
pub struct Workers<K, R> {
    /// Names this owner in the one report a worker that did not complete produces.
    context: &'static str,
    held: HashMap<K, Held<R>>,
    /// What each running task will be reported as. Kept here rather than returned by the task, because a
    /// task that did not run to completion returns nothing and still has a record to settle.
    running: HashMap<Id, (K, u64)>,
    tasks: JoinSet<Ended>,
    next: u64,
    /// The logical maximum: how many records may be held at once. Both maps are built at it and the charge
    /// covers it; admitting past it is refused rather than grown, and a retirement frees a slot.
    prepared: usize,
}

/// There is no identity left to issue. Fails closed: nothing is admitted, nothing is started, and no number
/// is reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exhausted;

/// Why an admission was refused, with the record handed back so the caller can unwind what it built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The fixed prepared bound is full. See [Workers::footprint].
    AtCapacity { prepared: usize },
    /// This key already names a live record.
    ///
    /// Refused rather than replaced, and refused *before* anything is started. A record only leaves through
    /// [Workers::retire], which needs its worker to have completed first, so overwriting one here would drop
    /// the existing `Held` - stranding whatever it owned, which for a descriptor-bearing record is a
    /// descriptor nothing will ever close - while its worker went on running against a row that no longer
    /// named it. Spawning first and refusing afterwards would be the same leak with a task attached, so the
    /// check precedes the spawn.
    Duplicate { id: u64 },
}

impl<K: Copy + Eq + Hash, R> Workers<K, R> {
    /// Prepares for `records` admitted records at once, which is the logical maximum [Workers::footprint]
    /// charged row state for and the one thing [Workers::admits] refuses on.
    pub fn with_capacity(context: &'static str, records: usize) -> Self {
        Self {
            context,
            // Requested at that maximum so the common case allocates nothing. An initial reservation, not a
            // promise either container owes: both may allocate or reorganise their own backing as they like,
            // which is count-bounded overhead rather than accounted state.
            held: HashMap::with_capacity(records),
            running: HashMap::with_capacity(records),
            tasks: JoinSet::new(),
            next: 0,
            prepared: records,
        }
    }

    /// What a table prepared for `records` owns, whatever is in it - charged once by the owner and kept
    /// charged until the table is rebuilt or dropped, because the charge follows the prepared bound rather
    /// than the rows currently in it.
    ///
    /// This is the two prepared owner maps and the record values in them, and that is all it claims to be.
    /// What it deliberately leaves out is the opaque runtime backing behind each admitted worker: the
    /// `JoinSet`'s task cell and list entry, and the `CancellationToken` node in [Workers::identity]. Those
    /// are **count-bounded rather than byte-charged**, which is an explicit category of the aggregate policy
    /// and not an omission and not something a caller charges instead - see the Shizuku design's
    /// *What Is Byte-Charged And What Is Count-Bounded*.
    ///
    /// Being precise about why, because the honest reason is a policy choice rather than an impossibility. A
    /// non-child `CancellationToken` node *is* fixed-layout in the pinned `tokio-util 0.7.19`: its `Inner`
    /// grows a children vector only for child tokens, which this daemon never makes, and the `Notify` beside
    /// it keeps its waiters intrusively inside the `Notified` futures rather than allocating them
    /// (`sync/notify.rs:215-217`). A `JoinSet` task cell is likewise a fixed shape for a given future. What
    /// neither exposes is a *supported* size: both layouts are crate-private, so any figure here would be a
    /// number read out of internals that the next version is free to reorganise. The policy's answer is to
    /// bound how many can exist instead, which this daemon can state exactly.
    ///
    /// How many can exist is this type's own invariant, and [Workers::bounded] is where it is stated:
    /// `running <= held <= prepared`, with one admitted worker owning one task bundle and one fresh non-child
    /// token. A record is reserved by its owner before [Workers::admit] is reached, so no cell here is ever
    /// constructed ahead of the grant that pays for its record.
    ///
    /// What that invariant covers is *admitted* records. It says nothing about a candidate identity
    /// [Workers::identity] has issued and whose admission has not returned yet, because this table cannot see
    /// one - and it does not need to: every owner that calls it is synchronous from `identity` to `admit`, so
    /// at most one candidate exists per owner and no second admission can begin before the first returns.
    pub fn footprint(records: usize) -> Option<u64> {
        logical_footprint::<(K, Held<R>)>(records)?
            .checked_add(logical_footprint::<(Id, (K, u64))>(records)?)?
            .checked_add(std::mem::size_of::<Self>() as u64)
    }

    /// Whether the count that stands in for a byte charge still holds: `running <= held <= prepared`.
    ///
    /// The opaque runtime cells this table owns - one task cell and list entry per running worker, one
    /// cancellation node per issued identity - are bounded by count rather than charged by size, so the
    /// count is the accounting and has to be checkable. Two facts make it one:
    ///
    /// - `held <= prepared`, because [Workers::admits] refuses rather than grows, and every held record was
    ///   paid for by its owner before this table was asked to take it;
    /// - `running <= held`, because a task is registered only alongside the row it belongs to, and a row is
    ///   only ever taken back *after* [Workers::finished] has reported that task and removed it - including
    ///   where an owner deliberately keeps the row after the report and takes it back much later, as a TCP
    ///   flow closing client-side does. Nothing else spawns here, and nothing spawns a child of a worker's
    ///   token.
    ///
    /// Both counts are over records this table admitted. A candidate identity an owner is holding between
    /// [Workers::identity] and [Workers::admit] is deliberately outside them - this table never sees one, and
    /// the bound on those is the owners' own synchrony rather than anything asserted here.
    ///
    /// Asserted rather than assumed at the three boundaries that could move either count - see
    /// [Workers::admit], [Workers::finished] and [Workers::retire].
    pub fn bounded(&self) -> bool {
        self.running.len() <= self.held.len() && self.held.len() <= self.prepared
    }

    /// The identity a record and its worker will share. Taken before either exists, because the worker is
    /// built from it and the record is checked against it.
    ///
    /// Checked, and a refusal rather than a wrap. Identities are what a terminal and every request a flow's
    /// worker makes of its owner are matched against, so reusing one is not a counter rolling over - it is
    /// a signal for a record that has been gone for a long time landing on whatever holds that number now.
    /// Wrapping would also reuse zero, which is the very first identity this table ever issued. A `u64`
    /// counter cannot be exhausted by any real workload; what it can be is exhausted by a bug, and a bug that
    /// fails closed is one that gets found.
    pub fn identity(&mut self) -> Result<Identity, Exhausted> {
        let id = self.next;
        self.next = self.next.checked_add(1).ok_or(Exhausted)?;
        Ok(Identity {
            id,
            cancel: CancellationToken::new(),
        })
    }

    /// Whether there is room for one more record, for an owner that cannot know its key until after it has
    /// committed to building something - a TCP flow's handle is issued by the socket set, not chosen.
    pub fn has_room(&self) -> bool {
        self.held.len() < self.prepared && self.running.len() < self.prepared
    }

    /// Whether a record may be admitted: no live record under this key, and a free slot in the logical bound.
    /// What an owner checks before it commits to anything a refusal would have to unwind.
    pub fn admits(&self, key: &K) -> Result<(), Refused> {
        if let Some(held) = self.held.get(key) {
            return Err(Refused::Duplicate { id: held.id });
        }
        // The prepared bound, on both maps, because a record needs a row in each and [Workers::admit] inserts
        // into both *after* it has spawned the task - by which point a refusal would be a task with no row to
        // settle it. A retirement takes both rows back and its slot is immediately the next admission's.
        // Neither map's `capacity()` is consulted: what those containers do with their own backing is theirs
        // to decide and is count-bounded rather than byte-charged - see
        // [crate::shared::admission::logical_footprint].
        if self.held.len() >= self.prepared || self.running.len() >= self.prepared {
            return Err(Refused::AtCapacity {
                prepared: self.prepared,
            });
        }
        Ok(())
    }

    /// Records one admission and starts its worker.
    ///
    /// The worker must own every task-local resource whose release a caller intends to prove by observing
    /// this task complete - a descriptor above all - so that completing is what releases it. That is not a
    /// rule that the record must be empty afterwards: state the *record* owns may outlive the terminal by
    /// design, and the TCP engine's client-closing phase is the case that does so, keeping a socket, a bridge
    /// and a grant on a row whose task has already been joined. The identity is borrowed rather than taken
    /// because the worker is built from it first: it has to name the same one, or a terminal would settle the
    /// wrong record.
    ///
    /// A key this table already holds is refused rather than replaced - see [Refused::Duplicate]. The record
    /// comes back on refusal, so the caller can unwind what it was built from rather than having it dropped
    /// here - which for a record holding an admission lease would leak that lease fail-closed.
    pub fn admit<F>(
        &mut self,
        key: K,
        identity: &Identity,
        record: R,
        worker: F,
    ) -> Result<(), (R, Refused)>
    where
        F: Future<Output = Ended> + Send + 'static,
    {
        // Checked before the task is spawned, not after: a worker started and then refused would be a task
        // nothing owns, holding whatever it was built from - and a duplicate that replaced its predecessor
        // would strand that predecessor's descriptor. `running` is bounded by `held` plus the tasks whose
        // records have already been retired, and a retired record's row is removed by [Workers::finished]
        // rather than left, so preparing both to the same capacity is enough.
        if let Err(why) = self.admits(&key) {
            return Err((record, why));
        }
        // One task bundle and one row, together. The token is the caller's own [Identity], issued once by
        // [Workers::identity] and never a child of another, so this worker's opaque runtime cells are the two
        // this line creates and the one that came with the identity - which is the count the aggregate policy
        // bounds in place of a byte charge.
        let task = self.tasks.spawn(worker);
        self.running.insert(task.id(), (key, identity.id));
        self.held.insert(
            key,
            Held {
                id: identity.id,
                cancel: identity.cancel.clone(),
                record,
            },
        );
        debug_assert!(
            self.bounded(),
            "one admitted worker owns one task bundle and one row"
        );
        Ok(())
    }

    pub fn get(&self, key: &K) -> Option<&Held<R>> {
        self.held.get(key)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut Held<R>> {
        self.held.get_mut(key)
    }

    pub fn contains(&self, key: &K) -> bool {
        self.held.contains_key(key)
    }

    /// Whether an event came from the worker this key currently holds, rather than from one already retired
    /// whose event was still in flight.
    ///
    /// **Both halves, always.** A key alone names a *slot*, and slots are reused - smoltcp hands a closed
    /// flow's `SocketHandle` straight back to the next one - so an answer that only asked whether the key
    /// existed would let a predecessor's terminal, request or retirement land on whatever holds that slot
    /// now. That is a live client's connection torn down by a connection that ended.
    pub fn current(&self, key: &K, id: u64) -> bool {
        self.held.get(key).is_some_and(|held| held.id == id)
    }

    /// Every record beside the key it is held under, for an owner whose identity needs both.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &Held<R>)> {
        self.held.iter()
    }

    pub fn values(&self) -> impl Iterator<Item = &Held<R>> {
        self.held.values()
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Held<R>> {
        self.held.values_mut()
    }

    /// How many records this table holds. Deliberately not how many *tasks* are running: a retired record's
    /// task has already been reported, and an owner may deliberately keep a row after its task has been
    /// reported - as a TCP flow closing client-side does - see [Workers::working] for the other count.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// Whether this table holds no record at all, which is not the same as holding no running worker.
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Whether any worker is still running. This is what a retirement loops on: a record is gone once its
    /// worker's terminal has been settled, so an owner that drains until nothing is running has both.
    pub fn working(&self) -> bool {
        !self.tasks.is_empty()
    }

    /// Asks one worker to stop. It is still running afterwards - only its terminal says otherwise.
    pub fn cancel(&self, key: &K) {
        if let Some(held) = self.held.get(key) {
            held.cancel.cancel();
        }
    }

    pub fn cancel_all(&self) {
        for held in self.held.values() {
            held.cancel.cancel();
        }
    }

    /// The next worker to have finished, or never when none is running.
    ///
    /// `Pending` rather than an immediate answer while the set is empty, because owners select on this: a set
    /// that answered at once would spin the loop it is selected in. Cancel-safe, since
    /// `JoinSet::poll_join_next_with_id` is, so an owner may abandon it for another arm and come back.
    pub async fn finished(&mut self) -> Terminal<K> {
        poll_fn(|cx| self.poll_finished(cx)).await
    }

    /// The same answer as a poll, for the owner that has to register several sources in one turn rather than
    /// hold a future for each - the TCP engine's `attention` is one.
    pub fn poll_finished(&mut self, cx: &mut Context<'_>) -> Poll<Terminal<K>> {
        loop {
            let (task, ended) = match self.tasks.poll_join_next_with_id(cx) {
                Poll::Ready(Some(Ok((task, ended)))) => (task, ended),
                Poll::Ready(Some(Err(e))) => (
                    e.id(),
                    Ended::Failed {
                        context: self.context,
                        error: io::Error::other(format!("a worker task did not complete: {e}")),
                    },
                ),
                // An empty set, which is nothing to wait *for* rather than nothing to wait *on*: no waker is
                // registered because nothing here can complete until this owner admits a worker, which is
                // work this same owner does between polls.
                Poll::Ready(None) | Poll::Pending => return Poll::Pending,
            };
            // Registered when the task was admitted and removed only here, so this answers for every task
            // this set can report. A completion for an unregistered task is not a record to settle, and
            // waiting for the next one is all that can honestly be done with it.
            if let Some((key, id)) = self.running.remove(&task) {
                debug_assert!(
                    self.bounded(),
                    "a reported task only lowers the running count"
                );
                return Poll::Ready(Terminal { key, id, ended });
            }
        }
    }

    /// Takes back the record a finished worker belonged to, which is what closes the descriptor the owner
    /// still held a share of. `None` when the identity is one this key no longer holds, which its successor
    /// must survive.
    ///
    /// Called only with the key and identity of a terminal [Workers::finished] produced, and that is what
    /// makes this the fence rather than a removal: the worker's task is complete by then, so whatever the
    /// record still owns is the last reference to it.
    pub fn retire(&mut self, key: &K, id: u64) -> Option<R> {
        if !self.current(key, id) {
            return None;
        }
        // Reached only with the identity of a terminal [Workers::finished] already reported, so this row's
        // task has left `running` before its row leaves `held`.
        let record = self.held.remove(key).map(|held| held.record);
        debug_assert!(
            self.bounded(),
            "a retired row leaves no task unaccounted for"
        );
        record
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::{copy_bidirectional_with_sizes, duplex, AsyncWriteExt};
    use tokio::time::timeout;

    use super::*;

    /// Long enough that no scheduling hiccup can trip it and short enough that a wait nobody will satisfy
    /// fails the run instead of hanging it. Every await below is bounded by it.
    const BOUND: Duration = Duration::from_secs(10);

    /// Says when a value was really dropped, so a test can tell "the task reported" from "the task's own
    /// resources are gone" - which is the whole of this module's fence.
    struct Witness(Arc<AtomicUsize>);

    impl Drop for Witness {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }

    fn dropped(counter: &Arc<AtomicUsize>) -> usize {
        counter.load(Ordering::Acquire)
    }

    /// A record standing where a flow's record stands: something whose release the owner may only do after
    /// the join.
    struct Record(u32);

    /// A worker that owns a witness and runs until it is cancelled, exactly as every dataplane worker does.
    async fn parked(witness: Witness, cancel: CancellationToken) -> Ended {
        cancel.cancelled().await;
        drop(witness);
        Ended::Expected
    }

    #[tokio::test]
    async fn a_terminal_means_the_task_is_gone_rather_than_that_it_said_so() {
        let mut workers: Workers<u32, Record> = Workers::with_capacity("test", 4);
        let counter = Arc::new(AtomicUsize::new(0));
        let identity = workers.identity().expect("a fresh table issues identities");
        let cancel = identity.cancel.clone();
        workers
            .admit(
                7,
                &identity,
                Record(7),
                parked(Witness(Arc::clone(&counter)), cancel),
            )
            .map_err(|(_, why)| why)
            .expect("the table was prepared for it");
        assert_eq!(dropped(&counter), 0);
        workers.cancel(&7);
        let terminal = timeout(BOUND, workers.finished())
            .await
            .expect("a cancelled worker finishes");
        assert_eq!(terminal.key, 7);
        assert_eq!(terminal.id, identity.id);
        // The fence: by the time the owner is told, tokio has dropped the future and everything it owned.
        // A table that reported a *message* from the worker would answer here with the witness still alive,
        // and the refund that follows would be claiming a descriptor is closed while it is open.
        assert_eq!(
            dropped(&counter),
            1,
            "the worker's own resources go before its owner is told"
        );
        // And the record is still the owner's until it asks for it back, which is what keeps the release
        // after the join rather than inside it.
        assert!(workers.contains(&7));
        let Record(record) = workers.retire(&7, terminal.id).expect("the live identity");
        assert_eq!(record, 7);
        assert!(!workers.contains(&7));
    }

    #[tokio::test]
    async fn a_worker_blocked_in_both_directions_still_finishes_when_it_is_cancelled() {
        let mut workers: Workers<u32, Record> = Workers::with_capacity("test", 4);
        let counter = Arc::new(AtomicUsize::new(0));
        let identity = workers.identity().expect("a fresh table issues identities");
        let cancel = identity.cancel.clone();
        // The two shapes a real flow's worker parks in, at once. Toward the bridge it is blocked *writing*:
        // the upstream has thirty-two bytes for it and the bridge will take four, and nobody drains the
        // bridge - which is the owner that has stopped draining in order to retire this very flow. Toward the
        // upstream it is blocked *reading*: it has moved everything the bridge had, and the upstream says
        // nothing more - which is a remote that has gone quiet. Neither wait is bounded by anything this
        // daemon decides, which is exactly why cancellation has to be able to end them.
        let (mut upstream, mut upstream_peer) = duplex(32);
        let (mut bridge, mut bridge_peer) = duplex(4);
        upstream_peer
            .write_all(&[0xau8; 32])
            .await
            .expect("the upstream's whole buffer");
        bridge_peer
            .write_all(&[0xbu8; 4])
            .await
            .expect("the bridge's whole buffer");
        let witness = Witness(Arc::clone(&counter));
        workers
            .admit(7, &identity, Record(7), async move {
                let held = witness;
                let ended = tokio::select! {
                    biased;
                    () = cancel.cancelled() => Ended::Expected,
                    _ = copy_bidirectional_with_sizes(&mut upstream, &mut bridge, 8, 8) => {
                        Ended::Reported("the copy ended on its own".to_owned())
                    }
                };
                drop(held);
                ended
            })
            .map_err(|(_, why)| why)
            .expect("the table was prepared for it");
        // Let the copy actually reach both of its waits before anything asks it to stop, so what cancellation
        // ends is a parked task rather than one that had not started.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        workers.cancel(&7);
        let terminal = timeout(BOUND, workers.finished())
            .await
            .expect("a cancelled copy finishes within the bound");
        assert!(matches!(terminal.ended, Ended::Expected));
        assert_eq!(
            dropped(&counter),
            1,
            "and its buffers and stream halves go with it, before the owner is told"
        );
        drop(upstream_peer);
        drop(bridge_peer);
    }

    #[tokio::test]
    async fn a_stale_identity_cannot_retire_the_successor_that_reused_its_key() {
        let mut workers: Workers<u32, Record> = Workers::with_capacity("test", 4);
        let predecessor = Arc::new(AtomicUsize::new(0));
        let successor = Arc::new(AtomicUsize::new(0));
        let first = workers.identity().expect("a fresh table issues identities");
        let cancel = first.cancel.clone();
        workers
            .admit(
                7,
                &first,
                Record(1),
                parked(Witness(Arc::clone(&predecessor)), cancel),
            )
            .map_err(|(_, why)| why)
            .expect("prepared");
        workers.cancel(&7);
        let terminal = timeout(BOUND, workers.finished())
            .await
            .expect("a cancelled worker finishes");
        workers.retire(&terminal.key, terminal.id).expect("live");

        // The slot is free and the next flow takes it, exactly as smoltcp hands a closed flow's handle back.
        let second = workers.identity().expect("identities are not reused");
        assert_ne!(second.id, first.id, "an identity is never reissued");
        let cancel = second.cancel.clone();
        workers
            .admit(
                7,
                &second,
                Record(2),
                parked(Witness(Arc::clone(&successor)), cancel),
            )
            .map_err(|(_, why)| why)
            .expect("the slot is free");

        // The predecessor's identity names nothing now, and may not touch the successor holding its key.
        // This is the mutation that matters: an answer that only asked whether the key existed would make
        // every line below succeed against the *successor*, tearing down a live client's flow because its
        // predecessor's event arrived late.
        assert!(!workers.current(&7, first.id));
        assert!(workers.current(&7, second.id));
        assert!(
            workers.retire(&7, first.id).is_none(),
            "a stale identity retires nothing"
        );
        assert!(
            workers.contains(&7),
            "and leaves the successor holding its key"
        );
        assert_eq!(
            dropped(&successor),
            0,
            "the successor's worker was never touched"
        );

        // And the successor then ends the way production ends one: cancel, join, and only then take the
        // record back - never a retirement that outruns its own task.
        workers.cancel(&7);
        let terminal = timeout(BOUND, workers.finished())
            .await
            .expect("the successor's worker finishes");
        assert_eq!(terminal.id, second.id);
        let Record(record) = workers.retire(&7, terminal.id).expect("the live identity");
        assert_eq!(record, 2, "the successor's record, never its predecessor's");
    }

    #[tokio::test]
    async fn a_key_a_live_worker_holds_is_refused_rather_than_replaced() {
        let mut workers: Workers<u32, Record> = Workers::with_capacity("test", 4);
        // One witness each, because the two things this proves are opposites: the candidate's future has to
        // die on the refusal - it was never spawned and nothing else will ever drop it - while the
        // predecessor's has to be left running untouched. One shared counter cannot tell those apart.
        let predecessor = Arc::new(AtomicUsize::new(0));
        let candidate = Arc::new(AtomicUsize::new(0));
        let first = workers.identity().expect("prepared");
        let cancel = first.cancel.clone();
        workers
            .admit(
                7,
                &first,
                Record(1),
                parked(Witness(Arc::clone(&predecessor)), cancel),
            )
            .map_err(|(_, why)| why)
            .expect("prepared");
        let second = workers.identity().expect("prepared");
        let cancel = second.cancel.clone();
        // The record comes back rather than being dropped here, because a record holding a lease dropped on
        // this path is a lease nothing releases.
        let (Record(refused), why) = workers
            .admit(
                7,
                &second,
                Record(2),
                parked(Witness(Arc::clone(&candidate)), cancel),
            )
            .expect_err("a live key is refused");
        assert_eq!(refused, 2);
        assert_eq!(why, Refused::Duplicate { id: first.id });
        assert!(
            workers.current(&7, first.id),
            "the predecessor is untouched"
        );
        assert_eq!(
            dropped(&predecessor),
            0,
            "a refusal stops nothing that was already running"
        );
        assert_eq!(
            dropped(&candidate),
            1,
            "and leaves nothing of the candidate behind: its future was never spawned"
        );
        workers.cancel_all();
        timeout(BOUND, workers.finished())
            .await
            .expect("the one live worker finishes");
        assert_eq!(dropped(&predecessor), 1, "and only then does it stop");
    }

    #[tokio::test]
    async fn the_prepared_bound_is_what_admission_refuses_on() {
        let prepared = 2usize;
        let mut workers: Workers<u32, Record> = Workers::with_capacity("test", prepared);
        // Separated for the same reason the duplicate refusal separates them: what a refusal at the bound has
        // to leave behind is nothing of the candidate's and everything of the admitted workers'.
        let admitted = Arc::new(AtomicUsize::new(0));
        let candidate = Arc::new(AtomicUsize::new(0));
        for key in 0..prepared as u32 {
            assert!(workers.has_room());
            let identity = workers.identity().expect("prepared");
            let cancel = identity.cancel.clone();
            workers
                .admit(
                    key,
                    &identity,
                    Record(key),
                    parked(Witness(Arc::clone(&admitted)), cancel),
                )
                .map_err(|(_, why)| why)
                .expect("inside the bound");
        }
        assert!(!workers.has_room());
        assert!(workers.bounded());
        let identity = workers.identity().expect("prepared");
        let cancel = identity.cancel.clone();
        let (_, why) = workers
            .admit(
                9,
                &identity,
                Record(9),
                parked(Witness(Arc::clone(&candidate)), cancel),
            )
            .expect_err("one past the bound is refused");
        assert_eq!(why, Refused::AtCapacity { prepared });
        assert_eq!(
            dropped(&admitted),
            0,
            "a refusal at the bound stops none of the workers already inside it"
        );
        assert_eq!(
            dropped(&candidate),
            1,
            "and nothing was spawned for the one it refused"
        );
        workers.cancel_all();
        for _ in 0..prepared {
            timeout(BOUND, workers.finished())
                .await
                .expect("every live worker finishes");
        }
        assert!(!workers.working());
        assert_eq!(dropped(&admitted), prepared);
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_table_waits_rather_than_answering() {
        let mut workers: Workers<u32, Record> = Workers::with_capacity("test", 4);
        // Nothing to wait *for* rather than nothing to wait *on*: an owner selecting on this must be left
        // pending, because an immediate answer would spin its loop for as long as it holds no worker.
        assert!(
            timeout(BOUND, workers.finished()).await.is_err(),
            "an empty table never completes"
        );
        assert!(!workers.working());
    }
}
