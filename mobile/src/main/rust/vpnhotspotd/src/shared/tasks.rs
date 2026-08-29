use std::future::Future;
use std::io;

use futures_util::future::select_all;
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

/// Folds two results without losing either failure.
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
    /// Result of reaping finished tasks before this admission.
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
    use crate::shared::protocol::{reported_io_error_report, IoErrorReportExt};

    fn folded(results: Vec<(&'static str, io::Result<()>)>) -> io::Result<()> {
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
        probes.close().await.expect("a closed owner closes quietly");
    }

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
            tokio::task::yield_now().await;
            assert!(probes.running().await <= 1);
        }
        assert_eq!(ran.load(Ordering::SeqCst), 64);
        probes.close().await.expect("nothing was left running");

        let probes = Background::new("control.ipsec_probe");
        assert!(probes.admit(|_| pending()).await.started);
        probes.abort_all().await;
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
        assert_eq!(both.kind(), io::ErrorKind::InvalidData);
        assert_eq!(both.to_string(), "ingress; the writer stopped");

        let described = combine(
            Err(io::Error::from_raw_os_error(libc::ENFILE).with_report_context("ingress")),
            Err(io::Error::from_raw_os_error(libc::EMFILE).with_report_context("writer")),
        )
        .unwrap_err();
        assert!(
            reported_io_error_report(&described).is_none(),
            "a folded message is not a structured report"
        );
    }

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
        assert_eq!(stopped.load(Ordering::SeqCst), 0);
        let shutdown = folded(tasks.shutdown().await);
        assert!(shutdown.is_ok());
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        assert!(cancel.is_cancelled());
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

    #[tokio::test]
    async fn a_task_that_did_not_complete_is_named_in_the_result() {
        let cancel = CancellationToken::new();
        let mut tasks = Tasks::new(cancel);
        let lost = tokio::spawn(async { pending::<io::Result<()>>().await });
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
        assert!(folded(tasks.shutdown().await).is_ok());
    }

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
