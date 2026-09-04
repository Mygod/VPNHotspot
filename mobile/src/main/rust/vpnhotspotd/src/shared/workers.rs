use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::hash::Hash;
use std::io;
use std::task::{Context, Poll};

use tokio::task::{Id, JoinSet};
use tokio_util::sync::CancellationToken;

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

/// One record its owner keeps, beside the identity and token of its worker.
pub struct Held<R> {
    pub id: u64,
    pub cancel: CancellationToken,
    pub record: R,
}

/// The records one owner admitted, and their tasks.
pub struct Workers<K, R> {
    /// Names this owner in the one report a worker that did not complete produces.
    context: &'static str,
    held: HashMap<K, Held<R>>,
    /// What each running task will be reported as. Kept here rather than returned by the task, because a
    /// task that did not run to completion returns nothing and still has a record to settle.
    running: HashMap<Id, (K, u64)>,
    tasks: JoinSet<Ended>,
    next: u64,
}

/// There is no identity left to issue. Fails closed: nothing is admitted, nothing is started, and no number
/// is reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exhausted;

/// Why an admission was refused, with the record handed back so the caller can unwind what it built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// This key already names a live record.
    Duplicate { id: u64 },
}

impl<K: Copy + Eq + Hash, R> Workers<K, R> {
    /// Builds an owner whose tables grow with admitted records.
    pub fn new(context: &'static str) -> Self {
        Self {
            context,
            held: HashMap::new(),
            running: HashMap::new(),
            tasks: JoinSet::new(),
            next: 0,
        }
    }

    /// Whether every running task still belongs to a held record.
    fn consistent(&self) -> bool {
        self.running.len() <= self.held.len()
    }

    /// The identity a record and its worker will share. Taken before either exists, because the worker is
    /// built from it and the record is checked against it.
    pub fn identity(&mut self) -> Result<Identity, Exhausted> {
        let id = self.next;
        self.next = self.next.checked_add(1).ok_or(Exhausted)?;
        Ok(Identity {
            id,
            cancel: CancellationToken::new(),
        })
    }

    /// Whether a record may be admitted: no live record may already use this key.
    pub fn admits(&self, key: &K) -> Result<(), Refused> {
        if let Some(held) = self.held.get(key) {
            return Err(Refused::Duplicate { id: held.id });
        }
        Ok(())
    }

    /// Records one admission and starts its worker.
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
        // could strand resources owned by either record.
        if let Err(why) = self.admits(&key) {
            return Err((record, why));
        }
        // One task bundle and one row, together. The token is the caller's own [Identity], issued once by
        // [Workers::identity] and never a child of another, so this worker's opaque runtime cells are the two
        // this line creates and the one that came with the identity.
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
            self.consistent(),
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
                    self.consistent(),
                    "a reported task only lowers the running count"
                );
                return Poll::Ready(Terminal { key, id, ended });
            }
        }
    }

    /// Takes back the record a finished worker belonged to. `None` when the identity is one this key no
    /// longer holds, which its successor must survive.
    pub fn retire(&mut self, key: &K, id: u64) -> Option<R> {
        if !self.current(key, id) {
            return None;
        }
        // Reached only with the identity of a terminal [Workers::finished] already reported, so this row's
        // task has left `running` before its row leaves `held`.
        let record = self.held.remove(key).map(|held| held.record);
        debug_assert!(
            self.consistent(),
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

    const BOUND: Duration = Duration::from_secs(10);

    struct Witness(Arc<AtomicUsize>);

    impl Drop for Witness {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }

    fn dropped(counter: &Arc<AtomicUsize>) -> usize {
        counter.load(Ordering::Acquire)
    }

    struct Record(u32);

    async fn parked(witness: Witness, cancel: CancellationToken) -> Ended {
        cancel.cancelled().await;
        drop(witness);
        Ended::Expected
    }

    #[tokio::test]
    async fn a_terminal_means_the_task_is_gone_rather_than_that_it_said_so() {
        let mut workers: Workers<u32, Record> = Workers::new("test");
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
            .expect("a fresh key is admitted");
        assert_eq!(dropped(&counter), 0);
        workers.cancel(&7);
        let terminal = timeout(BOUND, workers.finished())
            .await
            .expect("a cancelled worker finishes");
        assert_eq!(terminal.key, 7);
        assert_eq!(terminal.id, identity.id);
        assert_eq!(
            dropped(&counter),
            1,
            "the worker's own resources go before its owner is told"
        );
        assert!(workers.contains(&7));
        let Record(record) = workers.retire(&7, terminal.id).expect("the live identity");
        assert_eq!(record, 7);
        assert!(!workers.contains(&7));
    }

    #[tokio::test]
    async fn a_worker_blocked_in_both_directions_still_finishes_when_it_is_cancelled() {
        let mut workers: Workers<u32, Record> = Workers::new("test");
        let counter = Arc::new(AtomicUsize::new(0));
        let identity = workers.identity().expect("a fresh table issues identities");
        let cancel = identity.cancel.clone();
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
            .expect("a fresh key is admitted");
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
        let mut workers: Workers<u32, Record> = Workers::new("test");
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
            .expect("a fresh key is admitted");
        workers.cancel(&7);
        let terminal = timeout(BOUND, workers.finished())
            .await
            .expect("a cancelled worker finishes");
        workers.retire(&terminal.key, terminal.id).expect("live");

        let second = workers.identity().expect("identities are not reused");
        assert_ne!(second.id, first.id);
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

        workers.cancel(&7);
        let terminal = timeout(BOUND, workers.finished())
            .await
            .expect("the successor's worker finishes");
        assert_eq!(terminal.id, second.id);
        let Record(record) = workers.retire(&7, terminal.id).expect("the live identity");
        assert_eq!(record, 2);
    }

    #[tokio::test]
    async fn a_key_a_live_worker_holds_is_refused_rather_than_replaced() {
        let mut workers: Workers<u32, Record> = Workers::new("test");
        let predecessor = Arc::new(AtomicUsize::new(0));
        let candidate = Arc::new(AtomicUsize::new(0));
        let first = workers.identity().expect("identity");
        let cancel = first.cancel.clone();
        workers
            .admit(
                7,
                &first,
                Record(1),
                parked(Witness(Arc::clone(&predecessor)), cancel),
            )
            .map_err(|(_, why)| why)
            .expect("a fresh key is admitted");
        let second = workers.identity().expect("identity");
        let cancel = second.cancel.clone();
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
        assert_eq!(dropped(&predecessor), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_table_waits_rather_than_answering() {
        let mut workers: Workers<u32, Record> = Workers::new("test");
        assert!(
            timeout(BOUND, workers.finished()).await.is_err(),
            "an empty table never completes"
        );
        assert!(!workers.working());
    }
}
