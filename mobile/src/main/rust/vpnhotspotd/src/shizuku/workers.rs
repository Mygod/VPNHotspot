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
    /// [Workers::admit], [Workers::finished] and [Workers::retire].
    pub(crate) fn bounded(&self) -> bool {
        self.running.len() <= self.held.len() && self.held.len() <= self.prepared
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
