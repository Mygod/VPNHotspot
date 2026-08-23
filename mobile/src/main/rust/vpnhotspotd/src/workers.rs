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

use std::collections::HashMap;
use std::future::{pending, Future};
use std::hash::Hash;
use std::io;

use tokio::task::{Id, JoinSet};
use tokio_util::sync::CancellationToken;

use vpnhotspotd::shared::admission::logical_footprint;
pub(crate) use vpnhotspotd::shared::ended::Ended;

/// One finished worker: which record it belonged to, and how it ended.
pub(crate) struct Terminal<K> {
    pub(crate) key: K,
    /// Which worker under that key. A terminal naming an identity the table has already replaced applies to
    /// nothing, and the owner has to be able to tell.
    pub(crate) id: u64,
    pub(crate) ended: Ended,
}

/// What a record and its worker share: the identity events are checked against, and the token that stops it.
pub(crate) struct Identity {
    pub(crate) id: u64,
    pub(crate) cancel: CancellationToken,
}

/// One record its owner keeps, beside the identity and token of the worker holding its descriptor.
pub(crate) struct Held<R> {
    pub(crate) id: u64,
    pub(crate) cancel: CancellationToken,
    pub(crate) record: R,
}

/// The records one owner admitted, and the tasks that hold their descriptors.
///
/// This exists as a type rather than as a `HashMap` beside a `JoinSet` in each owner because the ordering
/// above is the correctness property, and four owners keeping it by hand is four places for it to drift.
pub(crate) struct Workers<K, R> {
    /// Names this owner in the one report a worker that did not complete produces.
    context: &'static str,
    held: HashMap<K, Held<R>>,
    /// What each running task will be reported as. Kept here rather than returned by the task, because a
    /// task that did not run to completion returns nothing and still has a record to settle.
    running: HashMap<Id, (K, u64)>,
    tasks: JoinSet<Ended>,
    next: u64,
    /// Set by a test to make the next admission - and only the next one - refuse. Never true in a build that
    /// is not a test harness.
    #[cfg(test)]
    refuse_next_admit: bool,
    /// The logical maximum: how many records may be held at once. Both maps are built at it and the charge
    /// covers it; admitting past it is refused rather than grown, and a retirement frees a slot.
    prepared: usize,
}

/// There is no identity left to issue. Fails closed: nothing is admitted, nothing is started, and no number
/// is reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Exhausted;

/// Why an admission was refused, with the record handed back so the caller can unwind what it built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Admitting would have to grow the tables holding the record, which is the owner's accounting to do
    /// rather than this type's to hide.
    ///
    /// Answered rather than done, because the growth is an accounting event: both maps exist beside their
    /// replacements while it happens, and that peak has to be charged before it and given back only after.
    /// See [Workers::footprint] and [Workers::grow_to].
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

// The table's whole surface, kept together. A binary that is not a test harness happens not to call every
// accessor - the proofs and the reconfiguration path do - and removing one because of that would be removing
// the answer to a question the owner is entitled to ask.
#[cfg_attr(not(test), allow(dead_code))]
impl<K: Copy + Eq + Hash, R> Workers<K, R> {
    /// Prepares for `records` admitted records at once, which is the logical maximum [Workers::footprint]
    /// charged row state for and the one thing [Workers::admits] refuses on.
    pub(crate) fn with_capacity(context: &'static str, records: usize) -> Self {
        Self {
            context,
            // Requested at that maximum so the common case allocates nothing. An initial reservation, not a
            // promise either container owes: both may allocate or reorganise their own backing as they like,
            // which is count-bounded overhead rather than accounted state.
            held: HashMap::with_capacity(records),
            running: HashMap::with_capacity(records),
            tasks: JoinSet::new(),
            next: 0,
            #[cfg(test)]
            refuse_next_admit: false,
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
    pub(crate) fn footprint(records: usize) -> Option<u64> {
        logical_footprint::<(K, Held<R>)>(records)?
            .checked_add(logical_footprint::<(Id, (K, u64))>(records)?)?
            .checked_add(std::mem::size_of::<Self>() as u64)
    }

    /// How many records this table is prepared for.
    pub(crate) fn prepared(&self) -> usize {
        self.prepared
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
    ///   only ever taken back *after* [Workers::finished] has reported that task and removed it - on the
    ///   detached path too, where the report comes first and the row is taken back much later. Nothing else
    ///   spawns here, and nothing spawns a child of a worker's token.
    ///
    /// Both counts are over records this table admitted. A candidate identity an owner is holding between
    /// [Workers::identity] and [Workers::admit] is deliberately outside them - this table never sees one, and
    /// the bound on those is the owners' own synchrony rather than anything asserted here.
    ///
    /// Asserted rather than assumed at the three boundaries that could move either count - see
    /// [Workers::admit], [Workers::finished] and [Workers::retire] - so a change that multiplied these cells
    /// per admitted record fails a test rather than a bound.
    pub(crate) fn bounded(&self) -> bool {
        self.running.len() <= self.held.len() && self.held.len() <= self.prepared
    }

    /// Makes the next admission refuse, once.
    ///
    /// A registration checks for room before it commits to building anything and admits afterwards, so the
    /// refusal that arrives *between* those two points is the one a single thread cannot otherwise produce -
    /// and it is the one whose unwind matters most, because everything the build took is in hand by then.
    /// Consumed by [Workers::admit] itself, at its own refusal point and before it spawns anything, so what
    /// the caller gets back is the ordinary typed refusal carrying its own record.
    #[cfg(test)]
    pub(crate) fn refuse_next_admit(&mut self) {
        self.refuse_next_admit = true;
    }

    /// Rebuilds both maps at a larger logical maximum, moving what is already here.
    ///
    /// Both sets of row state exist while the move runs, which is why the owner charges the replacement beside
    /// the original and commits only afterwards. A request that is not actually larger is refused, so a
    /// rollback cannot shrink a bound a charge still covers. The `JoinSet` is untouched: no task is started,
    /// stopped or reordered by this.
    pub(crate) fn grow_to(&mut self, records: usize) -> bool {
        if records <= self.prepared {
            return false;
        }
        // The new logical maximum, which is what the charge for this replacement covered.
        let mut held = HashMap::with_capacity(records);
        held.extend(self.held.drain());
        let mut running = HashMap::with_capacity(records);
        running.extend(self.running.drain());
        self.held = held;
        self.running = running;
        self.prepared = records;
        true
    }

    /// The identity a record and its worker will share. Taken before either exists, because the worker is
    /// built from it and the record is checked against it.
    ///
    /// Checked, and a refusal rather than a wrap. Identities are what a terminal, a readiness marker and a
    /// delivery acknowledgment are all matched against, so reusing one is not a counter rolling over - it is
    /// a signal for a record that has been gone for a long time landing on whatever holds that number now.
    /// Wrapping would also reuse zero, which is the very first identity this table ever issued. A `u64`
    /// counter cannot be exhausted by any real workload; what it can be is exhausted by a bug, and a bug that
    /// fails closed is one that gets found.
    pub(crate) fn identity(&mut self) -> Result<Identity, Exhausted> {
        let id = self.next;
        self.next = self.next.checked_add(1).ok_or(Exhausted)?;
        Ok(Identity {
            id,
            cancel: CancellationToken::new(),
        })
    }

    /// Winds the identity cursor forward, so exhaustion can be reached without issuing 2^64 identities.
    ///
    /// A method rather than a field poke, so what a test forces is a state this table can really be in: the
    /// cursor only ever moves forward here too, and everything downstream of it - `identity`, `admit`,
    /// `current` - runs exactly as it does at any other cursor value.
    #[cfg(test)]
    pub(crate) fn wind_to(&mut self, next: u64) {
        assert!(next >= self.next, "the cursor never moves backwards");
        self.next = next;
    }

    /// Records one admission and starts its worker.
    ///
    /// The worker must own everything whose close this record stands for, so that completing is what closes
    /// it. The identity is borrowed rather than taken because the worker is built from it first: it has to
    /// name the same one, or a terminal would settle the wrong record.
    ///
    /// A key this table already holds is refused rather than replaced - see [Refused::Duplicate].
    /// Whether there is room for one more record, for an owner that cannot know its key until after it has
    /// committed to building something - a TCP flow's handle is issued by the socket set, not chosen.
    pub(crate) fn has_room(&self) -> bool {
        self.held.len() < self.prepared && self.running.len() < self.prepared
    }

    /// Whether a record may be admitted: no live record under this key, and a free slot in the logical bound.
    /// What an owner checks before it commits to anything a refusal would have to unwind.
    pub(crate) fn admits(&self, key: &K) -> Result<(), Refused> {
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

    /// The record comes back on refusal, so the caller can unwind what it was built from rather than having
    /// it dropped here - which for a record holding an admission lease would leak that lease fail-closed.
    pub(crate) fn admit<F>(
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
        #[cfg(test)]
        if std::mem::take(&mut self.refuse_next_admit) {
            return Err((
                record,
                Refused::AtCapacity {
                    prepared: self.prepared,
                },
            ));
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

    pub(crate) fn get(&self, key: &K) -> Option<&Held<R>> {
        self.held.get(key)
    }

    pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut Held<R>> {
        self.held.get_mut(key)
    }

    pub(crate) fn contains(&self, key: &K) -> bool {
        self.held.contains_key(key)
    }

    /// Whether an event came from the worker this key currently holds, rather than from one already retired
    /// whose event was still in flight.
    pub(crate) fn current(&self, key: &K, id: u64) -> bool {
        self.held.get(key).is_some_and(|held| held.id == id)
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.held.keys()
    }

    /// Every record beside the key it is held under, for an owner whose identity needs both.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &Held<R>)> {
        self.held.iter()
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &Held<R>> {
        self.held.values()
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut Held<R>> {
        self.held.values_mut()
    }

    pub(crate) fn len(&self) -> usize {
        self.held.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Whether any worker is still running. This is what a retirement loops on: a record is gone once its
    /// worker's terminal has been settled, so an owner that drains until nothing is running has both.
    pub(crate) fn working(&self) -> bool {
        !self.tasks.is_empty()
    }

    /// Asks one worker to stop. It is still running afterwards - only its terminal says otherwise.
    pub(crate) fn cancel(&self, key: &K) {
        if let Some(held) = self.held.get(key) {
            held.cancel.cancel();
        }
    }

    pub(crate) fn cancel_all(&self) {
        for held in self.held.values() {
            held.cancel.cancel();
        }
    }

    /// The next worker to have finished, or never when none is running.
    ///
    /// `pending` rather than an immediate `None`, because owners select on this: a set that answered at once
    /// while empty would spin the loop it is selected in. Cancel-safe, since `JoinSet::join_next_with_id`
    /// is, so an owner may abandon it for another arm and come back.
    pub async fn finished(&mut self) -> Terminal<K> {
        loop {
            if self.tasks.is_empty() {
                pending::<()>().await;
            }
            let (task, ended) = match self.tasks.join_next_with_id().await {
                Some(Ok((task, ended))) => (task, ended),
                Some(Err(e)) => (
                    e.id(),
                    Ended::Failed {
                        context: self.context,
                        error: io::Error::other(format!("a worker task did not complete: {e}")),
                    },
                ),
                // the set was not empty a moment ago and this is its only consumer, so this is a race with
                // nothing rather than a completion to report
                None => continue,
            };
            // Registered when the task was admitted and removed only here, so this answers for every task
            // this set can report. A completion for an unregistered task is not a record to settle, and
            // waiting for the next one is all that can honestly be done with it.
            if let Some((key, id)) = self.running.remove(&task) {
                debug_assert!(
                    self.bounded(),
                    "a reported task only lowers the running count"
                );
                return Terminal { key, id, ended };
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
    pub(crate) fn retire(&mut self, key: &K, id: u64) -> Option<R> {
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
    use std::sync::{Arc, Mutex};
    use vpnhotspotd::shared::admission::{Admission, Totals};
    use vpnhotspotd::shared::dns_debt::{self, Connection};

    use tokio::sync::mpsc;

    use super::*;

    /// Stands in for whatever a worker owns whose drop is the close: a socket, an `Arc<AsyncFd>`, a stream
    /// half. Records its own drop where the test can see it, which is the property the fence is about.
    #[derive(Debug)]
    struct Sentinel {
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for Sentinel {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The count that stands in for a byte charge holds at every boundary that could break it.
    ///
    /// The opaque runtime cells this table owns - a task cell and list entry per running worker, a
    /// cancellation node per admitted identity - are bounded by count rather than charged by size, so the
    /// count *is* the accounting. What could break it is a row admitted past the prepared bound, a task
    /// spawned without a row, or a row taken back while its task went unaccounted for. Each is checked here,
    /// and [Workers::bounded] is asserted on the production path at all three.
    ///
    /// The refused admission below is also where the candidate-identity limit shows itself, as far as it can
    /// be shown: the identity issued for it is never admitted, and because this owner is synchronous no second
    /// admission can begin while it is held. That is a property of the caller, not something `bounded()`
    /// counts, and this test does not pretend otherwise.
    #[tokio::test]
    async fn the_runtime_cells_stay_bounded_by_the_records_that_paid_for_them() {
        let prepared = 3usize;
        let mut workers: Workers<u8, ()> = Workers::with_capacity("test.bounded", prepared);
        assert!(workers.bounded());

        let mut identities = Vec::new();
        for key in 0..prepared as u8 {
            let identity = workers.identity().expect("an identity");
            let cancel = identity.cancel.clone();
            assert!(
                workers
                    .admit(key, &identity, (), async move {
                        cancel.cancelled().await;
                        Ended::Expected
                    })
                    .is_ok(),
                "record {key} fits"
            );
            identities.push(identity);
            assert!(workers.bounded());
        }
        assert_eq!(workers.held.len(), prepared);
        assert_eq!(workers.running.len(), prepared, "one task each, not more");

        // One past the bound is refused rather than grown, so no fourth *task* and no fourth admitted row is
        // created for it. Its candidate cancellation node does exist - `identity()` below makes one, and that
        // is the single synchronous candidate the policy permits - which is precisely why the bound is stated
        // over admitted records rather than over every node this table has ever handed out.
        let extra = workers.identity().expect("an identity");
        let (record, refused) = workers
            .admit(3, &extra, (), async { Ended::Expected })
            .expect_err("the table is full");
        assert_eq!(record, ());
        assert!(matches!(refused, Refused::AtCapacity { .. }));
        assert_eq!(workers.running.len(), prepared, "and nothing was spawned");
        assert!(workers.bounded());

        // And the ordering that makes `running <= held` hold rather than being hoped for: a task is reported
        // first and only then may its row be taken back, so `running` is always the lower of the two.
        for identity in &identities {
            identity.cancel.cancel();
        }
        let terminal = workers.finished().await;
        assert!(workers.bounded());
        assert!(workers.retire(&terminal.key, terminal.id).is_some());
        assert!(
            workers.bounded(),
            "the row is gone and its task is reported"
        );
        while !workers.held.is_empty() {
            let terminal = workers.finished().await;
            workers.retire(&terminal.key, terminal.id);
            assert!(workers.bounded());
        }
        assert_eq!(workers.running.len(), 0, "every task accounted for");
        assert!(!workers.working(), "and none still running");
    }

    /// A retired record gives its slot back, and a fresh identity takes it - the whole fence, repeated.
    ///
    /// Cancel, wait for the completion, retire, admit a fresh identity in its place. Both maps are exercised
    /// because a record needs a row in each, and [Workers::admits] gates a *new* record on the prepared bound:
    /// the logical maximum, which is what the charge covers and the only thing a retirement gives back. What
    /// the two containers do with their own backing across a session's worth of that is opaque count-bounded
    /// overhead and nothing this test reads.
    #[tokio::test]
    async fn a_retired_record_leaves_its_slot_for_the_next() {
        let prepared = 4usize;
        let mut workers: Workers<u32, ()> = Workers::with_capacity("test.churn", prepared);

        // Every worker parks on its own token, so nothing finishes until this test says which one does.
        let admit = |workers: &mut Workers<u32, ()>, key: u32| {
            let identity = workers.identity().expect("an identity");
            let cancel = identity.cancel.clone();
            workers.admit(key, &identity, (), async move {
                cancel.cancelled().await;
                Ended::Expected
            })
        };
        let mut next_key = 0u32;
        let mut live: Vec<u32> = Vec::new();
        while workers.has_room() {
            let key = next_key;
            next_key += 1;
            assert!(admit(&mut workers, key).is_ok(), "record {key} fits");
            live.push(key);
        }
        assert_eq!(live.len(), prepared, "the bound admits its whole count");
        assert!(
            admit(&mut workers, next_key).is_err(),
            "one past the bound is refused rather than allocated"
        );
        assert!(workers.bounded());

        // The fence, over and over: cancel, join, retire, re-admit. A retirement gives its slot back and the
        // next admission takes it, for as long as a session runs.
        for round in 0..2 * prepared {
            let retiring = live.remove(0);
            workers.cancel(&retiring);
            let terminal = workers.finished().await;
            assert_eq!(
                terminal.key, retiring,
                "round {round}: the cancelled worker is the one that ended"
            );
            assert!(workers.retire(&terminal.key, terminal.id).is_some());
            assert_eq!(
                workers.len(),
                prepared - 1,
                "round {round}: the slot came back"
            );
            let key = next_key;
            next_key += 1;
            admit(&mut workers, key)
                .unwrap_or_else(|_| panic!("round {round}: a retired slot is available again"));
            live.push(key);
            assert_eq!(
                workers.len(),
                prepared,
                "round {round}: the bound is full again"
            );
            assert!(workers.bounded(), "round {round}");
        }

        workers.cancel_all();
        while !workers.is_empty() {
            let terminal = workers.finished().await;
            workers.retire(&terminal.key, terminal.id);
        }
        assert!(!workers.working(), "every task was accounted for");
    }

    #[tokio::test]
    async fn a_terminal_means_the_worker_already_dropped_what_it_owned() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut workers: Workers<u8, ()> = Workers::with_capacity("test.worker", 16);
        let identity = workers.identity().expect("an identity");
        let cancel = identity.cancel.clone();
        let held = Sentinel {
            dropped: Arc::clone(&dropped),
        };
        let _ = workers.admit(7, &identity, (), async move {
            let sentinel = held;
            cancel.cancelled().await;
            drop(sentinel);
            Ended::Expected
        });
        assert!(workers.working());
        workers.cancel_all();
        let terminal = workers.finished().await;
        assert_eq!(terminal.key, 7);
        // The refund an owner performs next may only follow this, so it has to be true here rather than
        // eventually.
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(!workers.working());
        assert!(workers.retire(&terminal.key, terminal.id).is_some());
        assert!(workers.is_empty());
    }

    #[tokio::test]
    async fn a_worker_blocked_on_a_full_channel_still_reaches_its_terminal() {
        // One slot and nobody reading it, which is the shape every payload channel is in once its owner has
        // stopped draining to retire.
        let (events, _events) = mpsc::channel::<u8>(1);
        let mut workers: Workers<u8, ()> = Workers::with_capacity("test.worker", 16);
        let identity = workers.identity().expect("an identity");
        let cancel = identity.cancel.clone();
        let _ = workers.admit(1, &identity, (), async move {
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Ended::Expected,
                    sent = events.send(0) => if sent.is_err() {
                        return Ended::Reported("the owner is gone".to_owned());
                    },
                }
            }
        });
        // Filled before the cancellation, so the worker is parked in `send` when it arrives.
        tokio::task::yield_now().await;
        workers.cancel_all();
        assert!(matches!(workers.finished().await.ended, Ended::Expected));
        assert!(!workers.working());
    }

    #[tokio::test]
    async fn retirement_joins_every_worker_and_settles_each_once() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut refunded = 0usize;
        let mut workers: Workers<u8, Sentinel> = Workers::with_capacity("test.worker", 16);
        for key in 0..8u8 {
            let identity = workers.identity().expect("an identity");
            let cancel = identity.cancel.clone();
            let held = Sentinel {
                dropped: Arc::clone(&dropped),
            };
            let _ = workers.admit(
                key,
                &identity,
                Sentinel {
                    dropped: Arc::clone(&dropped),
                },
                async move {
                    let sentinel = held;
                    cancel.cancelled().await;
                    drop(sentinel);
                    Ended::Expected
                },
            );
        }
        workers.cancel_all();
        while workers.working() {
            let terminal = workers.finished().await;
            // The worker's share is gone; taking the record drops the owner's, and only then is the refund
            // true.
            let record = workers.retire(&terminal.key, terminal.id);
            assert!(record.is_some());
            drop(record);
            refunded += 1;
        }
        assert_eq!(refunded, 8);
        assert!(workers.is_empty());
        // Both shares of all eight, so nothing is outstanding once a retirement returns.
        assert_eq!(dropped.load(Ordering::SeqCst), 16);
    }

    /// The session's own end, where the mix is what makes it interesting: one worker parked on an owner that
    /// has stopped reading, one parked on a peer that says nothing, and one that finished a while ago and was
    /// never settled. None of them may be left running, and all of them must be settled exactly once.
    #[tokio::test]
    async fn a_session_that_ends_leaves_nothing_running() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let (events, _events) = mpsc::channel::<u8>(1);
        let mut workers: Workers<u8, Sentinel> = Workers::with_capacity("test.worker", 16);
        let sentinel = |dropped: &Arc<AtomicUsize>| Sentinel {
            dropped: Arc::clone(dropped),
        };
        let blocked = workers.identity().expect("an identity");
        let blocked_cancel = blocked.cancel.clone();
        let blocked_share = sentinel(&dropped);
        let _ = workers.admit(0, &blocked, sentinel(&dropped), async move {
            let share = blocked_share;
            loop {
                tokio::select! {
                    biased;
                    () = blocked_cancel.cancelled() => {
                        drop(share);
                        return Ended::Expected;
                    }
                    sent = events.send(0) => if sent.is_err() {
                        drop(share);
                        return Ended::Expected;
                    },
                }
            }
        });
        let idle = workers.identity().expect("an identity");
        let idle_cancel = idle.cancel.clone();
        let idle_share = sentinel(&dropped);
        let _ = workers.admit(1, &idle, sentinel(&dropped), async move {
            let share = idle_share;
            idle_cancel.cancelled().await;
            drop(share);
            Ended::Expected
        });
        let done = workers.identity().expect("an identity");
        let done_share = sentinel(&dropped);
        let _ = workers.admit(2, &done, sentinel(&dropped), async move {
            drop(done_share);
            Ended::Reported("the peer closed".to_owned())
        });
        // Long enough for the finished worker to have finished and the blocked one to be parked in its send.
        tokio::task::yield_now().await;
        workers.cancel_all();
        let mut settled = Vec::new();
        while workers.working() {
            let terminal = workers.finished().await;
            assert!(workers.retire(&terminal.key, terminal.id).is_some());
            settled.push(terminal.key);
        }
        settled.sort_unstable();
        assert_eq!(settled, [0, 1, 2]);
        assert!(workers.is_empty());
        assert_eq!(dropped.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn a_worker_that_ends_on_its_own_settles_through_the_same_path() {
        let mut workers: Workers<u8, ()> = Workers::with_capacity("test.worker", 16);
        let identity = workers.identity().expect("an identity");
        let _ = workers.admit(3, &identity, (), async {
            Ended::Reported("the peer closed".to_owned())
        });
        let terminal = workers.finished().await;
        assert!(matches!(terminal.ended, Ended::Reported(_)));
        assert!(workers.retire(&terminal.key, terminal.id).is_some());
        // Exactly once: a second settlement of the same terminal must find nothing, or the refund would be
        // charged back twice.
        assert!(workers.retire(&terminal.key, terminal.id).is_none());
    }

    #[tokio::test]
    async fn a_terminal_for_a_replaced_identity_retires_nothing() {
        let mut workers: Workers<u8, u32> = Workers::with_capacity("test.worker", 16);
        let first = workers.identity().expect("an identity");
        let stale = Terminal {
            key: 5,
            id: first.id,
            ended: Ended::Expected,
        };
        drop(first);
        let second = workers.identity().expect("an identity");
        let _ = workers.admit(5, &second, 99, async { Ended::Expected });
        assert!(workers.retire(&stale.key, stale.id).is_none());
        assert_eq!(workers.get(&5).map(|held| held.record), Some(99));
        let terminal = workers.finished().await;
        assert_eq!(workers.retire(&terminal.key, terminal.id), Some(99));
    }

    #[tokio::test]
    async fn a_worker_that_did_not_complete_is_still_reported_and_settled() {
        let mut workers: Workers<u8, u32> = Workers::with_capacity("test.worker", 16);
        let identity = workers.identity().expect("an identity");
        let _ = workers.admit(2, &identity, 42, async {
            panic!("a worker's own invariant broke")
        });
        let terminal = workers.finished().await;
        match terminal.ended {
            Ended::Failed { context, ref error } => {
                assert_eq!(context, "test.worker");
                assert!(error.to_string().contains("did not complete"));
            }
            _ => panic!("a lost worker must not look like an ordinary end"),
        }
        // Settled anyway: the task is gone either way, and leaving the record would strand its charge.
        assert_eq!(workers.retire(&terminal.key, terminal.id), Some(42));
        assert!(!workers.working());
    }

    /// The order an owner must keep, recorded where a test can read it back. Mirrors what the dataplane's
    /// owners do: drop the record, refund what it was charged, and only then let the acknowledgement go.
    #[tokio::test]
    async fn an_owner_refunds_and_acknowledges_only_after_the_join() {
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        struct Owned {
            log: Arc<Mutex<Vec<&'static str>>>,
            what: &'static str,
        }
        impl Drop for Owned {
            fn drop(&mut self) {
                self.log
                    .lock()
                    .expect("the log is poisoned")
                    .push(self.what);
            }
        }
        let mut workers: Workers<u8, Owned> = Workers::with_capacity("test.worker", 16);
        let identity = workers.identity().expect("an identity");
        let cancel = identity.cancel.clone();
        let worker_share = Owned {
            log: Arc::clone(&log),
            what: "worker dropped its share",
        };
        let _ = workers.admit(
            0,
            &identity,
            Owned {
                log: Arc::clone(&log),
                what: "owner dropped its share",
            },
            async move {
                let share = worker_share;
                cancel.cancelled().await;
                drop(share);
                Ended::Expected
            },
        );
        workers.cancel_all();
        let mut charged = 1usize;
        while workers.working() {
            let terminal = workers.finished().await;
            log.lock().expect("the log is poisoned").push("terminal");
            drop(workers.retire(&terminal.key, terminal.id));
            charged -= 1;
            log.lock().expect("the log is poisoned").push("refunded");
        }
        log.lock()
            .expect("the log is poisoned")
            .push("acknowledged");
        assert_eq!(charged, 0);
        assert_eq!(
            log.lock().expect("the log is poisoned").as_slice(),
            [
                "worker dropped its share",
                "terminal",
                "owner dropped its share",
                "refunded",
                "acknowledged",
            ]
        );
    }

    /// Admission stops at the prepared bound rather than growing either map, and the refusal names what it
    /// was prepared for so the owner can charge the growth before retrying.
    ///
    /// The refusal is what is asserted, not a `capacity()` reading afterwards: that number is a documented
    /// *lower* bound on what still fits, so it cannot testify about what was allocated. What keeps the bound
    /// honest is that the refusal happens at all - see [Workers::admits].
    #[tokio::test]
    async fn admission_at_prepared_capacity_is_refused_rather_than_allocated() {
        let mut workers: Workers<u8, ()> = Workers::with_capacity("test.worker", 2);
        for key in 0..2u8 {
            let identity = workers.identity().expect("an identity");
            workers
                .admit(key, &identity, (), async { Ended::Expected })
                .expect("prepared");
        }
        let identity = workers.identity().expect("an identity");
        assert_eq!(
            workers
                .admit(9, &identity, (), async { Ended::Expected })
                .expect_err("at capacity")
                .1,
            Refused::AtCapacity { prepared: 2 }
        );
        assert_eq!(workers.len(), 2, "a refusal registers nothing");
        assert!(
            workers.bounded(),
            "and both maps still agree with the bound"
        );

        // Growth is a real replacement, and only ever upward: a request that is not larger is refused so a
        // rollback cannot shrink an allocation a charge still covers.
        assert!(!workers.grow_to(2));
        assert!(!workers.grow_to(1));
        assert!(workers.grow_to(8));
        assert_eq!(workers.prepared(), 8);
        assert_eq!(workers.len(), 2, "growth carried the records across");
        let identity = workers.identity().expect("an identity");
        workers
            .admit(9, &identity, (), async { Ended::Expected })
            .expect("room now");
        assert_eq!(workers.len(), 3);
    }

    /// An emptied table keeps its bound and takes the whole of it again.
    ///
    /// Ordinary reuse: every record retires, every slot goes back to the bound, and the bound admits its whole
    /// count a second time. The charge follows the prepared bound rather than the rows in it, so nothing is
    /// refunded on the way out and nothing extra is owed on the way back in - and what the two maps do with
    /// their own backing in between is opaque count-bounded overhead nothing here reads.
    #[tokio::test]
    async fn an_emptied_table_takes_its_whole_bound_again() {
        let mut workers: Workers<u8, ()> = Workers::with_capacity("test.worker", 4);
        for key in 0..4u8 {
            let identity = workers.identity().expect("an identity");
            workers
                .admit(key, &identity, (), async { Ended::Expected })
                .expect("prepared");
        }
        while workers.working() {
            let terminal = workers.finished().await;
            workers.retire(&terminal.key, terminal.id);
        }
        assert!(workers.is_empty());
        assert_eq!(
            workers.prepared(),
            4,
            "the bound the charge covers is untouched by what left"
        );

        // The whole bound again, and one past it still refused.
        for key in 0..4u8 {
            let identity = workers.identity().expect("an identity");
            workers
                .admit(key, &identity, (), async { Ended::Expected })
                .expect("a retired slot is available again");
        }
        assert_eq!(workers.len(), 4);
        let identity = workers.identity().expect("an identity");
        assert_eq!(
            workers
                .admit(9, &identity, (), async { Ended::Expected })
                .expect_err("at capacity")
                .1,
            Refused::AtCapacity { prepared: 4 },
            "and the bound still refuses the next one"
        );
        assert!(workers.bounded());

        workers.cancel_all();
        while workers.working() {
            let terminal = workers.finished().await;
            workers.retire(&terminal.key, terminal.id);
        }
    }

    /// The charge is both maps' row state, and it is monotone and refused rather than wrapped.
    ///
    /// Row state, not allocation: whatever these two containers keep around their rows is count-bounded
    /// rather than charged, so this figure is deliberately below what they really take. What it must get right
    /// is the *row type* of each map - a `(K, V)` pair rather than the two sizes added, which loses whatever
    /// padding sits between them - and that both maps are counted, since a record needs a row in each.
    #[test]
    fn the_charge_is_both_maps_row_state() {
        for prepared in [0usize, 1, 3, 7, 8, 64, 129, 1_000] {
            let charged = Workers::<u8, u64>::footprint(prepared).expect("a chargeable capacity");
            let held = std::mem::size_of::<(u8, Held<u64>)>() as u64;
            let running = std::mem::size_of::<(Id, (u8, u64))>() as u64;
            assert_eq!(
                charged,
                prepared as u64 * (held + running) + std::mem::size_of::<Workers<u8, u64>>() as u64,
                "prepared {prepared}"
            );
        }
        assert!(
            Workers::<u8, u64>::footprint(65) > Workers::<u8, u64>::footprint(64),
            "monotone, which is what a solver walking capacities depends on"
        );
        assert_eq!(Workers::<u8, u64>::footprint(usize::MAX), None);
    }

    /// A key this table already holds is refused before anything starts: the existing record and its task
    /// are untouched, and nothing new is spawned.
    ///
    /// The failure this closes is not hypothetical. `HashMap::insert` returns the displaced value, and
    /// dropping it here would drop whatever that record owned - for a UDP mapping, the owner's share of a
    /// descriptor whose worker is still running against a row that no longer names it, so the terminal finds
    /// nothing to retire and the descriptor is never closed.
    #[tokio::test]
    async fn a_duplicate_key_is_refused_without_replacing_or_spawning_anything() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut workers: Workers<u8, Sentinel> = Workers::with_capacity("test.worker", 8);
        let first = workers.identity().expect("an identity");
        let hold = first.cancel.clone();
        workers
            .admit(
                7,
                &first,
                Sentinel {
                    dropped: Arc::clone(&dropped),
                },
                async move {
                    hold.cancelled().await;
                    Ended::Expected
                },
            )
            .expect("the first admission");
        assert_eq!(workers.len(), 1);
        assert!(workers.working());

        // A second admission under the same key, with its own identity and its own record.
        let second = workers.identity().expect("an identity");
        let (record, why) = workers
            .admit(
                7,
                &second,
                Sentinel {
                    dropped: Arc::clone(&dropped),
                },
                async { panic!("a refused admission must not start a task") },
            )
            .expect_err("the key is taken");
        assert_eq!(why, Refused::Duplicate { id: first.id });
        // The refusal handed the record back rather than dropping it, so nothing it owned was lost here.
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        drop(record);
        assert_eq!(dropped.load(Ordering::SeqCst), 1, "the caller drops it");

        // The original record and its identity are exactly as they were, and only its task is running.
        assert_eq!(workers.len(), 1);
        assert!(workers.current(&7, first.id));
        assert!(!workers.current(&7, second.id));
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "the original record was not displaced"
        );

        // And exactly one task exists: cancelling the first drains the set completely.
        first.cancel.cancel();
        let terminal = workers.finished().await;
        assert_eq!(terminal.key, 7);
        assert_eq!(terminal.id, first.id);
        workers.retire(&terminal.key, terminal.id);
        assert!(!workers.working(), "no second task was ever spawned");
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }

    /// Identity allocation fails closed at exhaustion, beside a live record that keeps working.
    ///
    /// A `u64` counter cannot be exhausted by any real workload; what it can be is exhausted by a bug, and a
    /// wrap would be the worst possible failure - identity zero is the *first* one this table ever issued, so
    /// reissuing it aims every stale terminal, readiness marker and delivery acknowledgment at whatever holds
    /// that number now. So the assertions here are about what an exhausted table does to the record it is
    /// already holding: nothing. No replacement, no removal, no reallocation of either map, and not one byte
    /// of the aggregate moved.
    #[tokio::test]
    async fn identity_allocation_fails_closed_without_disturbing_a_live_record() {
        let mut admission = Admission::new(Totals {
            admission_id: 1,
            record_total: 16,
            dns_record_floor: 0,
            byte_total: 1 << 20,
            reserved_byte_floor: 1 << 16,
            fragment_cap: 1 << 16,
            dns_token_cap: 4,
            byte_only_owners: 4,
        })
        .expect("the fixture totals hold their own accounting");
        // The aggregate's own fixed overhead, which is what "every byte back" is measured against.
        let empty = admission.bytes_charged();
        let mut workers: Workers<u8, Connection> = Workers::with_capacity("test.worker", 8);

        // One live record, holding a real grant and running a real task.
        let first = workers.identity().expect("an identity");
        let held = dns_debt::open(&mut admission, 4_096, false).expect("granted");
        let running = first.cancel.clone();
        workers
            .admit(7, &first, held, async move {
                running.cancelled().await;
                Ended::Expected
            })
            .expect("prepared");
        assert!(workers.working());
        let live = (admission.records_charged(), admission.bytes_charged());
        assert_eq!(live.0, 1, "the live record's grant");

        // Wound to the last identity that can be issued: the counter has to be advanceable *past* the one it
        // hands out, so `u64::MAX - 1` is the last usable number.
        workers.wind_to(u64::MAX - 1);
        let last = workers.identity().expect("one left");
        assert_eq!(last.id, u64::MAX - 1);
        assert_eq!(workers.next, u64::MAX);

        // And it is admitted through the real path, so the exhausted table below is holding two live records
        // rather than one plus a number nobody used.
        let second = dns_debt::open(&mut admission, 4_096, false).expect("granted");
        let running = last.cancel.clone();
        workers
            .admit(9, &last, second, async move {
                running.cancelled().await;
                Ended::Expected
            })
            .expect("prepared");
        assert_eq!(workers.len(), 2);
        let live = (admission.records_charged(), admission.bytes_charged());
        assert_eq!(live.0, 2, "both records' grants");

        // Exhausted. Every further request is refused, repeatedly - and none of it touches what is already
        // here.
        for attempt in 0..16 {
            assert_eq!(
                workers.identity().err(),
                Some(Exhausted),
                "attempt {attempt}"
            );
            assert_eq!(workers.next, u64::MAX, "attempt {attempt}: no wrap");
            assert_eq!(workers.len(), 2, "attempt {attempt}: nothing was removed");
            assert!(
                workers.current(&7, first.id),
                "attempt {attempt}: nor either replaced"
            );
            assert!(workers.current(&9, last.id), "attempt {attempt}");
            assert_eq!(
                (admission.records_charged(), admission.bytes_charged()),
                live,
                "attempt {attempt}: and not a byte moved"
            );
            assert!(workers.bounded(), "attempt {attempt}: and the bound holds");
        }
        // Two tasks, still. A refusal that spawned would show up here.
        assert!(workers.working());
        assert_eq!(workers.running.len(), 2);

        // And both live records retire normally afterwards, so this refused an allocation rather than a
        // table.
        workers.cancel_all();
        let mut retired = 0usize;
        while workers.working() {
            let terminal = workers.finished().await;
            let connection = workers
                .retire(&terminal.key, terminal.id)
                .expect("the join");
            dns_debt::close(&mut admission, connection, None).expect("closed idle");
            retired += 1;
        }
        assert_eq!(retired, 2, "both, and neither twice");
        assert!(workers.is_empty());
        assert!(!workers.working());
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.bytes_charged(), empty, "and every byte back");
        assert_eq!(
            workers.prepared(),
            8,
            "and the bound the charge covered is where it was"
        );
        assert_eq!(admission.invariant_violations(), 0);

        // A fresh table still issues from zero - the value a wrap would have collided with.
        let mut fresh: Workers<u8, ()> = Workers::with_capacity("test.worker", 2);
        assert_eq!(fresh.identity().expect("an identity").id, 0);
    }
}
