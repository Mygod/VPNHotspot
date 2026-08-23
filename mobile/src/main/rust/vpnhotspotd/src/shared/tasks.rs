//! Task ownership for a conversation: the ones it has to watch, and the ones it must not lose track of.
//!
//! Work runs in tasks whose descriptors and state outlive the statement that spawned them, and a handle that
//! is dropped rather than owned turns two different situations into one: a task that failed looks exactly
//! like a task that is still running, and a session can then return - or hand its dataplane over - while
//! something it started is still mutating state or writing frames.
//!
//! So a handle is owned in one of two ways here and never in a third.
//!
//! [Tasks] is for work the session's own life depends on: its result is part of the session's result, and its
//! *completion* is an event the session selects on, because a dataplane that died must not wait for a quiet
//! control socket to notice. Each handle leaves the set exactly once, whether it completed on its own or was
//! joined by the shutdown, which is what makes double-awaiting one impossible rather than merely unlikely.
//! The app-UID TestNetwork session is its only user.
//!
//! [Background] is for work a session starts but does not wait on until it stops: one token cancels all of it,
//! and closing joins it and refuses anything later - so a probe cannot mutate retired state or report into a
//! conversation that has already finished. Retained is not the same as accumulated, though: a handle whose
//! task has already finished is joined at the next admission, so what is held is what is *running* rather than
//! everything that ever ran. Root's IPsec probes are its only user.

use std::future::Future;
use std::io;

use futures_util::future::select_all;
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

/// Folds two results without losing either failure.
///
/// A session that failed *and* could not shut down cleanly is not the same thing as either on its own, and an
/// `io::Result` can only carry one error - so the second is folded into the first's message rather than
/// discarded by an `and`. The kind stays the first one's, because that is the failure the session is about.
pub fn combine(first: io::Result<()>, second: io::Result<()>) -> io::Result<()> {
    match (first, second) {
        (Ok(()), second) => second,
        (Err(first), Ok(())) => Err(first),
        (Err(first), Err(second)) => {
            Err(io::Error::new(first.kind(), format!("{first}; {second}")))
        }
    }
}

/// Why a session's wait ended.
pub enum Watched {
    /// The conversation was cancelled - by the control writer failing, or by the owner itself.
    Cancelled,
    /// One owned task completed on its own, and here is its result. The handle is gone from the set, so this
    /// result exists exactly once.
    Finished {
        name: &'static str,
        result: io::Result<()>,
    },
}

/// The tasks one conversation owns and must observe.
pub struct Tasks {
    cancel: CancellationToken,
    /// In the order they must be joined, and each present at most once: a completion removes its handle here
    /// rather than leaving it for a second await.
    running: Vec<(&'static str, JoinHandle<io::Result<()>>)>,
}

impl Tasks {
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            cancel,
            running: Vec::new(),
        }
    }

    /// Takes ownership of one already-spawned task under the name its failure will be reported as.
    pub fn admit(&mut self, name: &'static str, task: JoinHandle<io::Result<()>>) {
        self.running.push((name, task));
    }

    /// Whatever happens first: this conversation being cancelled, or one of its tasks finishing.
    ///
    /// Selected on by the session loop beside its control socket, which is the point. A control socket can be
    /// quiet for as long as the peer has nothing to say, so a dataplane task that failed or exited would
    /// otherwise not be noticed until the peer spoke again - the session would keep acknowledging configs
    /// against tasks that are gone.
    ///
    /// Cancel-safe: an abandoned wait leaves every handle where it was, so the owner may come back to it.
    pub async fn watch(&mut self) -> Watched {
        let cancel = self.cancel.clone();
        if self.running.is_empty() {
            cancel.cancelled().await;
            return Watched::Cancelled;
        }
        let finished = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            (result, index, _) = select_all(self.running.iter_mut().map(|(_, task)| task)) => {
                Some((index, result))
            }
        };
        match finished {
            None => Watched::Cancelled,
            Some((index, result)) => {
                // Removed rather than swapped out, so the remaining order is still the admission order the
                // shutdown below joins in.
                let (name, _) = self.running.remove(index);
                Watched::Finished {
                    name,
                    result: joined(name, result),
                }
            }
        }
    }

    /// Cancels the conversation and joins every task still running, in the order they were admitted.
    ///
    /// The one cleanup path: whatever ended the session - a setup step that failed after the first spawn, a
    /// task that died, the peer going away - arrives here, so there is one place that owns "nothing this
    /// conversation started is still running" rather than one per exit.
    /// Cancels and joins everything still running, one result per task.
    ///
    /// Per task rather than folded, because folding is lossy in the one way that matters here: [combine]
    /// builds a fresh error from two messages, which drops both errnos and any structured report already
    /// attached to them. The owner has to describe these before it folds them, so it needs them apart.
    pub async fn shutdown(&mut self) -> Vec<(&'static str, io::Result<()>)> {
        self.cancel.cancel();
        // Taken out first, so a handle is never awaited twice even if this future is itself abandoned: what
        // is left of the set is empty, and the handles go with this frame.
        let mut results = Vec::with_capacity(self.running.len());
        for (name, task) in std::mem::take(&mut self.running) {
            results.push((name, joined(name, task.await)));
        }
        results
    }
}

/// One task's outcome, named. A task that did not run to completion still has to say so: it is gone either
/// way, and the session's result is the only place left that can report it.
fn joined(name: &str, result: Result<io::Result<()>, tokio::task::JoinError>) -> io::Result<()> {
    match result {
        Ok(result) => result,
        Err(e) => Err(io::Error::other(format!("{name} task failed: {e}"))),
    }
}

/// Background tasks a process owns: started from anywhere, cancelled and joined together.
///
/// For work whose result nothing waits on while it runs but whose *end* has to precede the owner's own -
/// a probe that mutates shared state and sends frames, most of all. A detached handle would let one of those
/// run past the point its state was retired, its reports could still be carried, or its conversation's writer
/// was closed.
pub struct Background {
    /// Names this owner in the report a task that did not complete produces.
    context: &'static str,
    cancel: CancellationToken,
    /// `None` once closed, which is what refuses a later admission: a task admitted then would have nothing
    /// left to join it.
    tasks: Mutex<Option<JoinSet<()>>>,
}

/// What one admission did.
pub struct Admitted {
    /// Whether the task was started. `false` means this owner is closed - the process is stopping - and
    /// nothing was constructed, let alone spawned.
    pub started: bool,
    /// What joining the already-finished tasks found, named as this owner's context. Reaped at every
    /// admission rather than at the close, because a task that has finished still occupies its handle: a
    /// long-lived conversation that started thousands of probes would otherwise hold every one of them until
    /// the process exited, and would learn only then that one of them did not run to completion.
    pub reaped: io::Result<()>,
}

impl Background {
    pub fn new(context: &'static str) -> Self {
        Self {
            context,
            cancel: CancellationToken::new(),
            tasks: Mutex::new(Some(JoinSet::new())),
        }
    }

    /// Admits one task, built from the token that [Background::close] cancels, and joins whatever had already
    /// finished. See [Admitted].
    ///
    /// The task is built here rather than passed in already-built so that its cancellation cannot be
    /// forgotten by a caller, and so that a refused admission never constructs it at all.
    pub async fn admit<M, F>(&self, task: M) -> Admitted
    where
        M: FnOnce(CancellationToken) -> F,
        F: Future<Output = ()> + Send + 'static,
    {
        match self.tasks.lock().await.as_mut() {
            Some(tasks) => {
                // Before the spawn, so the one being admitted is not a candidate for its own reaping.
                let reaped = self.reap(tasks);
                tasks.spawn(task(self.cancel.clone()));
                Admitted {
                    started: true,
                    reaped,
                }
            }
            None => Admitted {
                started: false,
                reaped: Ok(()),
            },
        }
    }

    /// How many handles this owner is holding, which is what "retained is not accumulated" means: a task that
    /// has been reaped is not one of these.
    #[cfg(test)]
    async fn running(&self) -> usize {
        self.tasks.lock().await.as_ref().map_or(0, JoinSet::len)
    }

    /// Ends every task without joining it, which is how a test produces one that did not run to completion -
    /// the daemon aborts the process on a panic, so a panicking task is not a thing it can have.
    #[cfg(test)]
    async fn abort_all(&self) {
        if let Some(tasks) = self.tasks.lock().await.as_mut() {
            tasks.abort_all();
        }
    }

    /// Joins every task that has already finished, without waiting for any that has not.
    fn reap(&self, tasks: &mut JoinSet<()>) -> io::Result<()> {
        let mut result = Ok(());
        while let Some(joined) = tasks.try_join_next() {
            result = combine(result, self.completed(joined));
        }
        result
    }

    /// One task's outcome. Completing is the ordinary case and says nothing; not running to completion is the
    /// owner's to report, because the task is gone either way and its handle was the only thing that knew.
    fn completed(&self, joined: Result<(), tokio::task::JoinError>) -> io::Result<()> {
        match joined {
            Ok(()) => Ok(()),
            Err(e) => Err(io::Error::other(format!(
                "a {} task did not complete: {e}",
                self.context
            ))),
        }
    }

    /// Cancels every task, joins all of them, and refuses anything afterwards.
    ///
    /// Holding the lock across the joins is deliberate: an admission racing this waits for it and is then
    /// refused, rather than slipping a task in behind the last join.
    pub async fn close(&self) -> io::Result<()> {
        self.cancel.cancel();
        let mut tasks = self.tasks.lock().await;
        let Some(mut running) = tasks.take() else {
            return Ok(());
        };
        let mut result = Ok(());
        while let Some(joined) = running.join_next().await {
            result = combine(result, self.completed(joined));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    /// What an owner does with [Tasks::shutdown]'s per-task results once it has described each: fold them
    /// into the one error the session returns.
    fn folded(results: Vec<(&'static str, io::Result<()>)>) -> io::Result<()> {
        // A loop rather than `fold`, and never `try_fold`: short-circuiting on the first error is exactly
        // what [combine] exists to avoid, since both failures have to survive into the message.
        let mut all = Ok(());
        for (_, result) in results {
            all = combine(all, result);
        }
        all
    }

    #[tokio::test]
    async fn every_background_task_is_joined_before_close_returns() {
        let probes = Background::new("control.ipsec_probe");
        let finished = Arc::new(AtomicUsize::new(0));
        for _ in 0..4 {
            let share = Arc::clone(&finished);
            let admitted = probes
                .admit(|cancel| async move {
                    cancel.cancelled().await;
                    share.fetch_add(1, Ordering::SeqCst);
                })
                .await;
            assert!(admitted.started);
            assert!(admitted.reaped.is_ok());
        }
        probes.close().await.expect("cancelled probes are quiet");
        assert_eq!(finished.load(Ordering::SeqCst), 4);
    }

    /// The race the owner exists for: a call that decided to probe just as the process is stopping. The
    /// admission has to be refused rather than started, or nothing would join it.
    #[tokio::test]
    async fn a_task_admitted_after_close_never_starts() {
        let probes = Background::new("control.ipsec_probe");
        let started = Arc::new(AtomicUsize::new(0));
        probes.close().await.expect("an empty owner closes quietly");
        let share = Arc::clone(&started);
        assert!(
            !probes
                .admit(|_| async move {
                    share.fetch_add(1, Ordering::SeqCst);
                })
                .await
                .started
        );
        tokio::task::yield_now().await;
        assert_eq!(started.load(Ordering::SeqCst), 0);
        // Idempotent, because a process may close on a path that already did.
        probes.close().await.expect("a closed owner closes quietly");
    }

    /// A conversation lasts as long as a hotspot does and starts a probe for every upstream change in it, so
    /// what this owner holds has to be what is *running*. A handle whose task finished is joined at the next
    /// admission - and if that task did not run to completion, that is where its owner finds out, rather than
    /// at a process exit that may be hours away.
    #[tokio::test]
    async fn finished_background_tasks_are_reaped_rather_than_accumulated() {
        let probes = Background::new("control.ipsec_probe");
        let ran = Arc::new(AtomicUsize::new(0));
        for _ in 0..64 {
            let share = Arc::clone(&ran);
            let admitted = probes
                .admit(|_| async move {
                    share.fetch_add(1, Ordering::SeqCst);
                })
                .await;
            assert!(admitted.started);
            admitted.reaped.expect("a probe that ran is quiet");
            // Each one has finished before the next is admitted, so every admission but the first has one to
            // reap: what is held never grows past the one in flight.
            tokio::task::yield_now().await;
            assert!(probes.running().await <= 1);
        }
        assert_eq!(ran.load(Ordering::SeqCst), 64);
        probes.close().await.expect("nothing was left running");

        // And a task that did not run to completion is surfaced at the admission that reaps it, named as this
        // owner's context rather than lost with the handle.
        let probes = Background::new("control.ipsec_probe");
        assert!(probes.admit(|_| pending()).await.started);
        probes.abort_all().await;
        // One turn for the runtime to drop what it was told to end, so the handle has an outcome to reap.
        tokio::task::yield_now().await;
        let failure = probes
            .admit(|_| async {})
            .await
            .reaped
            .expect_err("a probe that did not complete is its owner's to report");
        assert!(failure
            .to_string()
            .contains("a control.ipsec_probe task did not complete"));
        probes.close().await.expect("the rest ran to completion");
    }

    /// A task shaped like every real one: it runs until its owner's token says otherwise. Nothing here
    /// aborts, because [Tasks::shutdown] joins rather than aborting - a task that ignored its token would
    /// hang the shutdown, which is the honest consequence of owning descriptors.
    fn cancellable(cancel: &CancellationToken) -> JoinHandle<io::Result<()>> {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            Ok(())
        })
    }

    #[test]
    fn combining_keeps_every_failure() {
        assert!(combine(Ok(()), Ok(())).is_ok());
        assert_eq!(
            combine(Ok(()), Err(io::Error::other("egress")))
                .unwrap_err()
                .to_string(),
            "egress"
        );
        assert_eq!(
            combine(Err(io::Error::other("ingress")), Ok(()))
                .unwrap_err()
                .to_string(),
            "ingress"
        );
        let both = combine(
            Err(io::Error::new(io::ErrorKind::InvalidData, "ingress")),
            Err(io::Error::other("the writer stopped")),
        )
        .unwrap_err();
        // Both accounts survive, and the kind is the one the session actually failed on.
        assert_eq!(both.kind(), io::ErrorKind::InvalidData);
        assert_eq!(both.to_string(), "ingress; the writer stopped");
    }

    /// The failure this exists for: one half of the dataplane dies while the control socket is quiet. The
    /// session has to see it, cancel the other half, and join both.
    #[tokio::test]
    async fn a_task_that_fails_on_its_own_is_seen_and_its_sibling_joined() {
        let cancel = CancellationToken::new();
        let mut tasks = Tasks::new(cancel.clone());
        let stopped = Arc::new(AtomicUsize::new(0));
        tasks.admit(
            "tun ingress",
            tokio::spawn(async { Err(io::Error::other("the TUN read failed")) }),
        );
        let sibling = Arc::clone(&stopped);
        let sibling_cancel = cancel.clone();
        tasks.admit(
            "tun egress",
            tokio::spawn(async move {
                sibling_cancel.cancelled().await;
                sibling.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        );
        let watched = tasks.watch().await;
        let Watched::Finished { name, result } = watched else {
            panic!("a dead dataplane must not read as a quiet conversation");
        };
        assert_eq!(name, "tun ingress");
        assert_eq!(
            result.as_ref().unwrap_err().to_string(),
            "the TUN read failed"
        );
        // Nothing has stopped the sibling yet: that is the owner's next move, not the watch's.
        assert_eq!(stopped.load(Ordering::SeqCst), 0);
        let shutdown = folded(tasks.shutdown().await);
        assert!(shutdown.is_ok());
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        assert!(cancel.is_cancelled());
        // Collected exactly once: the handle left the set with its result, so the shutdown had nothing of it
        // left to await - a second await of the same handle would have panicked here.
        assert_eq!(
            combine(result, shutdown).unwrap_err().to_string(),
            "the TUN read failed"
        );
    }

    #[tokio::test]
    async fn a_task_that_ends_cleanly_still_ends_the_session() {
        let cancel = CancellationToken::new();
        let mut tasks = Tasks::new(cancel.clone());
        tasks.admit("tun ingress", tokio::spawn(async { Ok(()) }));
        tasks.admit("tun egress", cancellable(&cancel));
        match tasks.watch().await {
            Watched::Finished { name, result } => {
                assert_eq!(name, "tun ingress");
                assert!(result.is_ok());
            }
            Watched::Cancelled => panic!("a task that returned is not a cancellation"),
        }
        assert!(folded(tasks.shutdown().await).is_ok());
    }

    /// A task that did not run to completion is gone either way, so the session's result is the only place
    /// left that can name it.
    #[tokio::test]
    async fn a_task_that_did_not_complete_is_named_in_the_result() {
        let cancel = CancellationToken::new();
        let mut tasks = Tasks::new(cancel);
        let lost = tokio::spawn(async { pending::<io::Result<()>>().await });
        // Aborted rather than panicked, because the daemon aborts the process on panic: either way the task
        // is gone without a result of its own, which is what the owner has to account for.
        lost.abort();
        tasks.admit("tun egress", lost);
        match tasks.watch().await {
            Watched::Finished { name, result } => {
                assert_eq!(name, "tun egress");
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("tun egress task failed"));
            }
            Watched::Cancelled => panic!("a lost task is not a cancellation"),
        }
        assert!(folded(tasks.shutdown().await).is_ok());
    }

    #[tokio::test]
    async fn cancellation_is_what_a_quiet_conversation_ending_looks_like() {
        let cancel = CancellationToken::new();
        let mut tasks = Tasks::new(cancel.clone());
        tasks.admit("tun ingress", cancellable(&cancel));
        tasks.admit("tun egress", cancellable(&cancel));
        cancel.cancel();
        assert!(matches!(tasks.watch().await, Watched::Cancelled));
        // Both are still owned and both are joined, in admission order, by the one cleanup path.
        assert!(folded(tasks.shutdown().await).is_ok());
    }

    /// The early-failure path: a setup step fails after the first task was spawned, so the session never
    /// starts and the cleanup still has to leave nothing running.
    #[tokio::test]
    async fn a_failure_before_the_loop_still_joins_what_was_started() {
        let cancel = CancellationToken::new();
        let mut tasks = Tasks::new(cancel.clone());
        let stopped = Arc::new(AtomicUsize::new(0));
        for name in ["tun ingress", "tun egress"] {
            let share = Arc::clone(&stopped);
            let token = cancel.clone();
            tasks.admit(
                name,
                tokio::spawn(async move {
                    token.cancelled().await;
                    share.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
            );
        }
        let setup: io::Result<()> = Err(io::Error::other("the budget could not be measured"));
        let result = combine(setup, folded(tasks.shutdown().await));
        assert_eq!(
            result.unwrap_err().to_string(),
            "the budget could not be measured"
        );
        assert_eq!(stopped.load(Ordering::SeqCst), 2);
    }
}
