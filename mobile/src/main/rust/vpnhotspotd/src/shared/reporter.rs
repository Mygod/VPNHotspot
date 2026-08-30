//! One control conversation's nonfatal coalescer and writer handoff.
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::shared::nonfatal::{NonfatalReport, SiteCoalescer};
use crate::shared::proto::daemon::DaemonErrorReport;

/// Where an emitted report goes, and whether it got there. `false` is a report the conversation could no
/// longer carry, which the finalizer turns into the conversation's own failure rather than losing.
type Sink = Box<dyn Fn(NonfatalReport, Handed) -> bool + Send + Sync>;

/// A reporter's share of its conversation's writer queue: how many reports may be waiting in it at once.
struct Handoff {
    available: AtomicUsize,
    /// Woken when a place comes back, which is the only thing that can un-stick a report the handoff had no
    /// room for. Nothing polls for it: the release is the event.
    freed: Notify,
}

impl Handoff {
    fn new(places: usize) -> Self {
        Self {
            available: AtomicUsize::new(places),
            freed: Notify::new(),
        }
    }

    /// Free producer places, read while the admission lock prevents competing producer reservations.
    fn available(&self) -> usize {
        self.available.load(Ordering::Acquire)
    }

    /// Takes one place, or `None` when the writer has none left.
    fn take(self: &Arc<Self>) -> Option<Handed> {
        self.available
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |available| {
                available.checked_sub(1)
            })
            .ok()
            .map(|_| Handed(Arc::clone(self)))
    }

    /// Waits for a place and takes it. No timeout: a place comes back when the writer drops the message it
    /// wrote, or when the writer goes away and drops what it was holding, and there is no third outcome worth
    /// inventing an answer for.
    async fn acquire(self: &Arc<Self>) -> Handed {
        loop {
            let freed = self.freed.notified();
            if let Some(place) = self.take() {
                return place;
            }
            freed.await;
        }
    }
}

/// One place in the writer's queue, owned by whatever is carrying the report it was handed with.
pub struct Handed(Arc<Handoff>);

impl Drop for Handed {
    /// Gives the place back and announces it in the same step, so the source that had no room for a report is
    /// woken by the writer's own progress rather than by a timer.
    fn drop(&mut self) {
        self.0.available.fetch_add(1, Ordering::AcqRel);
        self.0.freed.notify_one();
    }
}

/// What became of one report, which is a thing the caller has to be able to act on rather than assume.
pub enum Pushed {
    /// Taken: emitted immediately, or coalesced until the writer handoff has room.
    Coalesced,
    /// Handed back, because the conversation that owned the reporter has finished. Nothing was coalesced,
    /// emitted or opened - so this is not a report in flight, it is a report with nowhere to go, and the
    /// caller is the one that knows where to write it.
    Closed(DaemonErrorReport),
}

pub struct Reporter {
    state: Arc<State>,
    cancel: CancellationToken,
    /// Everything about ending this reporter, under one lock: the drain task, whether a finalizer has been
    /// started, and the answer it left. One lock because those three are one decision - "has somebody already
    /// taken responsibility for shutting this down, and if so what did they conclude".
    shutdown: Mutex<Shutdown>,
    /// Woken once [Shutdown::outcome] is set. Registered before the outcome is read, so a completion landing
    /// between the two is not missed.
    finished: Notify,
}

#[derive(Default)]
struct Shutdown {
    /// Taken by the finalizer that runs, and only by it. A cancelled `finish` waiter never touches it, so a
    /// dropped future cannot detach the task the conversation's result depends on.
    drainer: Option<JoinHandle<()>>,
    /// Whether a finalizer task has been spawned. Exactly one ever is.
    running: bool,
    /// What that finalizer concluded, or `None` while it has not concluded anything. A message rather than an
    /// `io::Error` because every waiter gets the same answer and `io::Error` is not cloneable.
    outcome: Option<Result<(), String>>,
}

struct State {
    /// The coalescer and whether it still admits reports, under one lock because those two have to be
    /// checked and acted on together: a report admitted after the final flush would stay pending forever,
    /// which is exactly the allocation a finished reporter must not make.
    admission: Mutex<Admission>,
    sink: Sink,
    /// This reporter's places in the control writer's queue - see the module note and [Handoff].
    handoff: Arc<Handoff>,
    /// Producers admitted before finalization that have not completed their handoff.
    emitting: AtomicUsize,
    /// Woken when [State::emitting] reaches zero, which is one of the two things the finalizer waits for
    /// besides its own task.
    drained: Notify,
    undelivered: AtomicUsize,
    /// Woken when the first source becomes pending; an existing pending set already waits for handoff room.
    opened: Notify,
}

struct Admission {
    coalescer: SiteCoalescer,
    closed: bool,
}

impl Reporter {
    /// Builds a reporter that holds no task yet.
    pub fn new(
        handoff: usize,
        sink: impl Fn(NonfatalReport, Handed) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            state: Arc::new(State {
                admission: Mutex::new(Admission {
                    coalescer: SiteCoalescer::new(),
                    closed: false,
                }),
                sink: Box::new(sink),
                handoff: Arc::new(Handoff::new(handoff)),
                emitting: AtomicUsize::new(0),
                drained: Notify::new(),
                undelivered: AtomicUsize::new(0),
                opened: Notify::new(),
            }),
            cancel: CancellationToken::new(),
            shutdown: Mutex::new(Shutdown::default()),
            finished: Notify::new(),
        }
    }

    /// Emits one report or retains it behind the occupied handoff. Never waits, so a packet path can call it.
    pub fn push(&self, call_id: Option<u64>, report: DaemonErrorReport) -> Pushed {
        let mut refused = None;
        let mut opened = false;
        self.state.emit(|admission, room| {
            if admission.closed {
                refused = Some(report);
                return Vec::new();
            }
            let (ready, wake) = admission.coalescer.push(call_id, report, room);
            opened = wake;
            ready
        });
        match refused {
            Some(report) => Pushed::Closed(report),
            None => {
                if opened {
                    self.state.opened.notify_one();
                }
                Pushed::Coalesced
            }
        }
    }

    /// The finalizer's answer, once there is one. Every waiter gets the same one.
    async fn terminal(&self) -> io::Result<()> {
        loop {
            // Registered before the read: [Notify::notify_waiters] wakes only what is already waiting, so a
            // finalizer completing between the two would otherwise leave this parked forever.
            let finished = self.finished.notified();
            if let Some(outcome) = self.locked_shutdown().outcome.clone() {
                return outcome.map_err(io::Error::other);
            }
            finished.await;
        }
    }

    fn locked_shutdown(&self) -> MutexGuard<'_, Shutdown> {
        self.shutdown
            .lock()
            .expect("the reporter's shutdown is poisoned")
    }

    /// Starts the pending-report drainer. Called by [ReporterRegistry::install] once this reporter is where producers
    /// can find it, which is the only order in which a failed installation spawns nothing.
    fn open(&self) {
        let task = tokio::spawn(drain_pending(Arc::clone(&self.state), self.cancel.clone()));
        let previous = self.locked_shutdown().drainer.replace(task);
        debug_assert!(previous.is_none());
    }
}

impl State {
    /// The admission lock, held for the coalescer's own bookkeeping and for taking places in the writer's
    /// queue, and never across an await. Poisoning cannot happen: the daemon aborts on panic.
    fn locked(&self) -> MutexGuard<'_, Admission> {
        self.admission
            .lock()
            .expect("the report coalescer is poisoned")
    }

    /// One emission: `ready` decides what to hand over, under the admission lock and against the room actually
    /// available, and the sink is then called outside it.
    fn emit(&self, ready: impl FnOnce(&mut Admission, usize) -> Vec<NonfatalReport>) {
        let taken = {
            let mut admission = self.locked();
            let reports = ready(&mut admission, self.handoff.available());
            if reports.is_empty() {
                return;
            }
            // Counted before the lock goes, which is the whole fence: a finalizer closes admission under this
            // same lock and then waits for this count, so it cannot slip between the decision and the sink.
            self.emitting.fetch_add(1, Ordering::Release);
            reports
                .into_iter()
                .map(|report| {
                    let place = self
                        .handoff
                        .take()
                        .expect("a report was emitted without a place in the writer's queue");
                    (report, place)
                })
                .collect::<Vec<_>>()
        };
        for (report, place) in taken {
            self.deliver(report, place);
        }
        // Released after the last send, so a finalization that saw this producer waited for all of it.
        if self.emitting.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.drained.notify_one();
        }
    }

    /// Waits until no producer is between the admission lock and the sink.
    async fn drain(&self) {
        while self.emitting.load(Ordering::Acquire) > 0 {
            self.drained.notified().await;
        }
    }

    fn deliver(&self, report: NonfatalReport, place: Handed) {
        if !(self.sink)(report, place) {
            self.undelivered.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Where a producer finds the process's reporter, and the one place a conversation's ownership of it is
/// recorded.
#[derive(Clone)]
pub struct ReporterRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    current: Mutex<Registration>,
}

/// What the process knows about reporting: who to find, and who is still on their way out.
struct Registration {
    /// What an ordinary lookup finds, and nothing else.
    current: Weak<Reporter>,
    /// The reporter whose finalization has begun and not completed. Held strongly, because this is the
    /// registry refusing an installation on its behalf rather than a way of reaching it; released by the
    /// finalizer that ran, and by nothing else.
    closing: Option<Arc<Reporter>>,
}

impl Default for ReporterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ReporterRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                current: Mutex::new(Registration {
                    current: Weak::new(),
                    closing: None,
                }),
            }),
        }
    }

    /// Installs one reporter and starts it, or refuses because another installation is still live - or is
    /// still finishing.
    pub fn install(&self, reporter: Reporter) -> io::Result<ReporterGuard> {
        let reporter = Arc::new(reporter);
        {
            let mut registration = self.inner.locked();
            if registration.current.strong_count() > 0 || registration.closing.is_some() {
                return Err(io::Error::other("nonfatal reporter already installed"));
            }
            registration.current = Arc::downgrade(&reporter);
        }
        reporter.open();
        Ok(ReporterGuard {
            registry: Arc::clone(&self.inner),
            reporter,
        })
    }

    /// The installed reporter, upgraded for as long as this caller holds it. `None` once the conversation
    /// that owned it has begun finishing, which is what a report made afterwards sees.
    pub fn get(&self) -> Option<Arc<Reporter>> {
        self.inner.locked().current.upgrade()
    }
}

impl RegistryInner {
    fn locked(&self) -> MutexGuard<'_, Registration> {
        self.current
            .lock()
            .expect("the reporter registration is poisoned")
    }

    /// Ends ordinary lookup and marks this reporter as still closing, in one step so that no successor can
    /// install into the gap between them. See [Registration].
    fn begin_finish(&self, reporter: &Arc<Reporter>) {
        let mut registration = self.locked();
        registration.clear_current(reporter);
        if registration.closing.is_none() {
            registration.closing = Some(Arc::clone(reporter));
        }
    }

    /// Gives up exactly this registration, and only if it is still the one closing: a conversation that
    /// finished after a successor installed its own must not invalidate the successor's. Called by the
    /// finalizer that ran, and by nothing else.
    fn release(&self, reporter: &Arc<Reporter>) {
        let mut registration = self.locked();
        registration.clear_current(reporter);
        if registration
            .closing
            .as_ref()
            .is_some_and(|closing| Arc::ptr_eq(closing, reporter))
        {
            registration.closing = None;
        }
    }
}

impl Registration {
    fn clear_current(&mut self, reporter: &Arc<Reporter>) {
        if std::ptr::eq(self.current.as_ptr(), Arc::as_ptr(reporter)) {
            self.current = Weak::new();
        }
    }
}

/// One conversation's ownership of the process reporter: the only strong reference outside the registry, and
/// the only thing that can start its finalization.
pub struct ReporterGuard {
    registry: Arc<RegistryInner>,
    reporter: Arc<Reporter>,
}

impl ReporterGuard {
    /// Ends reporting for this conversation: ordinary lookup stops at once and the registration stays busy
    /// until the finalizer's ordered shutdown has completed - see [finalize], whose result this is.
    pub async fn finish(self) -> io::Result<()> {
        ensure_finalized(&self.registry, &self.reporter);
        self.reporter.terminal().await
    }
}

impl Drop for ReporterGuard {
    /// Makes sure the same finalizer has been started, and nothing else.
    fn drop(&mut self) {
        ensure_finalized(&self.registry, &self.reporter);
    }
}

/// Starts the one finalizer for this reporter, if it has not been started already.
fn ensure_finalized(registry: &Arc<RegistryInner>, reporter: &Arc<Reporter>) {
    let mut shutdown = reporter.locked_shutdown();
    // Already over. Marking the registration again here would take it back on behalf of a conversation that
    // has finished - which is exactly what a guard's `Drop` does after its own `finish` returned, and would
    // refuse every successor for the rest of the process. Checked under the same lock the finalizer stores
    // its outcome and releases the registration under, so there is no gap to land in.
    if shutdown.outcome.is_some() {
        return;
    }
    registry.begin_finish(reporter);
    if shutdown.running {
        return;
    }
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(runtime) => runtime,
        Err(_) => {
            // Nothing here can own a finalizer, so nothing here may pretend one ran. Admission is closed and
            // the drain task cancelled so that nothing accumulates or waits for a handoff nobody will drain;
            // the handle is *not* taken, so it is neither detached nor joined by anyone but a real finalizer,
            // and `closing` is *not* released - which leaves this registry permanently refusing successors.
            // Fail-closed, and visibly so.
            drop(shutdown);
            reporter.state.locked().closed = true;
            reporter.cancel.cancel();
            return;
        }
    };
    shutdown.running = true;
    let drainer = shutdown.drainer.take();
    drop(shutdown);
    let registry = Arc::clone(registry);
    let reporter = Arc::clone(reporter);
    runtime.spawn(finalize(registry, reporter, drainer));
}

/// Ends one conversation's reporting, in the one order that makes each step's promise true of the next.
async fn finalize(
    registry: Arc<RegistryInner>,
    reporter: Arc<Reporter>,
    drainer: Option<JoinHandle<()>>,
) {
    let last = {
        let mut admission = reporter.state.locked();
        admission.closed = true;
        admission.coalescer.flush()
    };
    reporter.cancel.cancel();
    reporter.state.drain().await;
    let mut failure = match drainer {
        Some(drainer) => drainer
            .await
            .err()
            .map(|e| format!("the pending-report drain task failed: {e}")),
        None => None,
    };
    for report in last {
        // One at a time, and the wait for the next place *is* the wait for the writer to have taken this one:
        // there is one place, so acquiring it again cannot succeed until the message carrying the last report
        // has been dropped. A writer that went away drops what it was holding, which releases the place and
        // makes the next send return the real failure rather than parking on a peer that is gone.
        let place = reporter.state.handoff.acquire().await;
        reporter.state.deliver(report, place);
    }
    let undelivered = reporter.state.undelivered.load(Ordering::Relaxed);
    if failure.is_none() && undelivered > 0 {
        failure = Some(format!(
            "{undelivered} nonfatal reports could not be delivered to the controller"
        ));
    }
    // Stored and released together, then announced. Together because a `Drop` arriving between the two would
    // see an unfinished reporter and mark the registration busy again on behalf of a conversation that is
    // over; announced afterwards because a waiter woken before the outcome existed would loop forever.
    {
        let mut shutdown = reporter.locked_shutdown();
        shutdown.outcome = Some(match failure {
            Some(failure) => Err(failure),
            None => Ok(()),
        });
        registry.release(&reporter);
    }
    reporter.finished.notify_waiters();
}

/// Gives each returned handoff place to the oldest blocked source. No clock participates: a source is pending
/// exactly while the writer handoff is occupied.
async fn drain_pending(state: Arc<State>, cancel: CancellationToken) {
    loop {
        // Ahead of the wait so a stored opened/freed notification cannot make a pending report wait through
        // another event. If anything remains, every handoff place was consumed and writer progress is the only
        // useful wake; otherwise the first blocked source is.
        state.emit(|admission, room| admission.coalescer.emit(room));
        let pending = state.locked().coalescer.has_pending();
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            () = state.handoff.freed.notified(), if pending => {}
            () = state.opened.notified(), if !pending => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    const WAIT: Duration = Duration::from_secs(1);
    const HANDOFF: usize = 1;

    fn report(context: &str, line: u32) -> DaemonErrorReport {
        DaemonErrorReport {
            context: context.to_owned(),
            message: "a mapping's receive failed".to_owned(),
            errno: Some(5),
            kind: "Other".to_owned(),
            file: "src/shizuku/udp.rs".to_owned(),
            line,
            column: 1,
            pid: 321,
            details: Vec::new(),
        }
    }

    #[allow(clippy::type_complexity)]
    fn collecting() -> (Arc<Mutex<Vec<NonfatalReport>>>, Sink) {
        let emitted: Arc<Mutex<Vec<NonfatalReport>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&emitted);
        (
            emitted,
            Box::new(move |report, permit| {
                drop(permit);
                sink.lock().expect("the sink is poisoned").push(report);
                true
            }),
        )
    }

    fn count(emitted: &Arc<Mutex<Vec<NonfatalReport>>>) -> usize {
        emitted.lock().expect("the sink is poisoned").len()
    }

    type Held = Arc<Mutex<Vec<(NonfatalReport, Handed)>>>;

    fn holding() -> (Held, Sink) {
        let held = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&held);
        (
            held,
            Box::new(move |report, place| {
                sink.lock()
                    .expect("the sink is poisoned")
                    .push((report, place));
                true
            }),
        )
    }

    fn suppressed(report: &NonfatalReport) -> Option<&str> {
        report
            .report
            .details
            .iter()
            .find(|detail| detail.key == "coalesced.suppressed_count")
            .map(|detail| detail.value.as_str())
    }

    #[tokio::test]
    async fn a_flood_retains_one_latest_summary_behind_the_occupied_handoff() {
        let registry = ReporterRegistry::new();
        let (held, sink) = holding();
        let reporter = registry
            .install(Reporter::new(HANDOFF, sink))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        assert!(matches!(
            pushing.push(None, report("shizuku.udp_send", 42)),
            Pushed::Coalesced
        ));
        for _ in 1..10_000 {
            assert!(matches!(
                pushing.push(None, report("shizuku.udp_send", 42)),
                Pushed::Coalesced
            ));
        }
        assert_eq!(held.lock().expect("the sink is poisoned").len(), 1);
        drop(pushing);
        held.lock().expect("the sink is poisoned").remove(0);
        reporter.finish().await.expect("the flush must complete");
        let held = held.lock().expect("the sink is poisoned");
        assert_eq!(held.len(), 1);
        assert_eq!(suppressed(&held[0].0), Some("9998"));
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_first_pending_source_wakes_the_drain_task() {
        let (held, sink) = holding();
        let reporter = Reporter::new(HANDOFF, sink);

        assert!(matches!(
            reporter.push(None, report("shizuku.udp_send", 42)),
            Pushed::Coalesced
        ));
        assert!(matches!(
            reporter.push(None, report("shizuku.udp_send", 42)),
            Pushed::Coalesced
        ));
        reporter.state.opened.notified().await;
        assert!(matches!(
            reporter.push(None, report("shizuku.udp_send", 77)),
            Pushed::Coalesced
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(1), reporter.state.opened.notified())
                .await
                .is_err(),
            "the drain task already owns the nonempty pending set",
        );
        assert_eq!(held.lock().expect("the sink is poisoned").len(), 1);
    }

    #[tokio::test]
    async fn returning_the_handoff_emits_the_latest_summary_and_finish_joins_the_drainer() {
        let registry = ReporterRegistry::new();
        let places: Arc<Mutex<Vec<Handed>>> = Arc::new(Mutex::new(Vec::new()));
        let (emitted, mut arriving) = tokio::sync::mpsc::unbounded_channel();
        let sink = Arc::clone(&places);
        let reporter = registry
            .install(Reporter::new(HANDOFF, move |report, place| {
                sink.lock().expect("the sink is poisoned").push(place);
                emitted.send(report).is_ok()
            }))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        for call_id in [7, 8, 9] {
            assert!(matches!(
                pushing.push(Some(call_id), report("shizuku.echo_send", 11)),
                Pushed::Coalesced
            ));
        }
        assert_eq!(
            arriving.recv().await.expect("the first report").call_id,
            Some(7)
        );
        assert!(arriving.try_recv().is_err());
        places.lock().expect("the sink is poisoned").remove(0);
        let summary = arriving
            .recv()
            .await
            .expect("the returned place wakes the batch");
        assert_eq!(summary.call_id, Some(9));
        assert_eq!(suppressed(&summary), Some("1"));
        places.lock().expect("the sink is poisoned").clear();
        reporter.finish().await.expect("the flush must complete");
        assert!(matches!(
            pushing.push(None, report("shizuku.echo_send", 11)),
            Pushed::Closed(_)
        ));
        assert!(arriving.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_pending_summary_is_flushed_exactly_once_when_the_conversation_finishes() {
        let registry = ReporterRegistry::new();
        let (held, sink) = holding();
        let reporter = registry
            .install(Reporter::new(HANDOFF, sink))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        for _ in 0..3 {
            assert!(matches!(
                pushing.push(None, report("shizuku.tun_output", 5)),
                Pushed::Coalesced
            ));
        }
        assert_eq!(held.lock().expect("the sink is poisoned").len(), 1);
        held.lock().expect("the sink is poisoned").remove(0);
        reporter.finish().await.expect("the flush must complete");
        let held = held.lock().expect("the sink is poisoned");
        assert_eq!(held.len(), 1);
        assert_eq!(suppressed(&held[0].0), Some("1"));
    }

    #[tokio::test]
    async fn nothing_is_reported_after_the_conversation_finished() {
        let registry = ReporterRegistry::new();
        let (emitted, sink) = collecting();
        let reporter = registry
            .install(Reporter::new(HANDOFF, sink))
            .expect("the first installation must be accepted");
        let racing = registry.get().expect("a producer finds the reporter");
        reporter.finish().await.expect("the flush must complete");
        assert!(registry.get().is_none());
        assert!(matches!(
            racing.push(Some(3), report("shizuku.udp_send", 42)),
            Pushed::Closed(_)
        ));
        assert_eq!(count(&emitted), 0);
        drop(racing);
        assert_eq!(count(&emitted), 0);
    }

    #[tokio::test]
    async fn a_second_installation_is_refused_and_leaves_nothing_running() {
        let registry = ReporterRegistry::new();
        let (emitted, sink) = collecting();
        let reporter = registry
            .install(Reporter::new(HANDOFF, sink))
            .expect("the first installation must be accepted");
        let (refused_emitted, refused_sink) = collecting();
        let refused = match registry.install(Reporter::new(HANDOFF, refused_sink)) {
            Ok(_) => panic!("a second conversation must not install its own reporter"),
            Err(e) => e,
        };
        assert!(refused.to_string().contains("already installed"));
        assert_eq!(Arc::strong_count(&refused_emitted), 1);
        assert_eq!(count(&refused_emitted), 0);
        registry
            .get()
            .expect("the first reporter is still the process's")
            .push(None, report("shizuku.tcp_sweep", 7));
        assert_eq!(count(&emitted), 1);
        reporter.finish().await.expect("the flush must complete");
        let (successor_emitted, successor_sink) = collecting();
        let successor = registry
            .install(Reporter::new(HANDOFF, successor_sink))
            .expect("a finished conversation releases the registration");
        registry
            .get()
            .expect("the successor is the process's reporter")
            .push(None, report("shizuku.tcp_sweep", 7));
        assert_eq!(count(&successor_emitted), 1);
        successor.finish().await.expect("the flush must complete");
    }

    #[tokio::test]
    async fn a_guard_dropped_without_finishing_ends_the_reporter() {
        let registry = ReporterRegistry::new();
        let (abandoned_emitted, sink) = collecting();
        let abandoned = {
            let _guard = registry
                .install(Reporter::new(HANDOFF, sink))
                .expect("the first installation must be accepted");
            let handle = registry.get().expect("a producer finds the reporter");
            for _ in 0..2 {
                assert!(matches!(
                    handle.push(None, report("shizuku.tun_output", 5)),
                    Pushed::Coalesced
                ));
            }
            handle
        };
        assert!(registry.get().is_none());
        abandoned
            .terminal()
            .await
            .expect("the guard's finalizer completes");
        assert!(matches!(
            abandoned.push(None, report("shizuku.tun_output", 5)),
            Pushed::Closed(_)
        ));
        assert_eq!(count(&abandoned_emitted), 2);
        drop(abandoned);
        let (emitted, sink) = collecting();
        let successor = registry
            .install(Reporter::new(HANDOFF, sink))
            .expect("an abandoned registration must not outlive its conversation");
        registry
            .get()
            .expect("the successor is the process's reporter")
            .push(None, report("shizuku.echo_socket", 12));
        assert_eq!(count(&emitted), 1);
        successor.finish().await.expect("the flush must complete");
    }

    #[tokio::test]
    async fn a_report_the_conversation_could_not_carry_is_returned_by_finish() {
        let registry = ReporterRegistry::new();
        let reporter = registry
            .install(Reporter::new(HANDOFF, |_: NonfatalReport, _| false))
            .expect("the first installation must be accepted");
        registry
            .get()
            .expect("a producer finds the reporter")
            .push(None, report("shizuku.tun_egress", 99));
        let failed = reporter
            .finish()
            .await
            .expect_err("an undelivered report has to reach the conversation's result");
        assert!(failed.to_string().contains("could not be delivered"));
    }

    #[test]
    fn a_guard_dropped_outside_a_runtime_leaves_the_registry_fail_closed() {
        let runtime = tokio::runtime::Runtime::new().expect("a runtime for the installation");
        let registry = ReporterRegistry::new();
        let (emitted, sink) = collecting();
        let guard = runtime.enter();
        let reporter = registry
            .install(Reporter::new(HANDOFF, sink))
            .expect("the first installation must be accepted");
        let handle = registry.get().expect("a producer finds the reporter");
        assert!(matches!(
            handle.push(None, report("shizuku.tun_output", 5)),
            Pushed::Coalesced
        ));
        drop(guard);
        drop(reporter);
        assert!(registry.get().is_none());
        assert!(matches!(
            handle.push(None, report("shizuku.tun_output", 5)),
            Pushed::Closed(_)
        ));
        assert_eq!(count(&emitted), 1);
        let (_, successor_sink) = collecting();
        assert!(registry
            .install(Reporter::new(HANDOFF, successor_sink))
            .is_err());
    }

    struct Parked {
        emitted: Arc<Mutex<Vec<NonfatalReport>>>,
        entered: mpsc::Receiver<()>,
        release: mpsc::Sender<()>,
        finished: Arc<AtomicBool>,
        late: Arc<AtomicUsize>,
    }

    #[allow(clippy::type_complexity)]
    fn parking() -> (Parked, Sink) {
        let emitted: Arc<Mutex<Vec<NonfatalReport>>> = Arc::new(Mutex::new(Vec::new()));
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let finished = Arc::new(AtomicBool::new(false));
        let late = Arc::new(AtomicUsize::new(0));
        let sink_emitted = Arc::clone(&emitted);
        let sink_finished = Arc::clone(&finished);
        let sink_late = Arc::clone(&late);
        let calls = AtomicUsize::new(0);
        let release_rx = Mutex::new(release_rx);
        (
            Parked {
                emitted,
                entered: entered_rx,
                release,
                finished,
                late,
            },
            Box::new(move |report, permit| {
                drop(permit);
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    entered.send(()).expect("the test is still waiting");
                    release_rx
                        .lock()
                        .expect("the release is poisoned")
                        .recv()
                        .expect("the test releases the producer");
                }
                if sink_finished.load(Ordering::SeqCst) {
                    sink_late.fetch_add(1, Ordering::SeqCst);
                }
                sink_emitted
                    .lock()
                    .expect("the sink is poisoned")
                    .push(report);
                true
            }),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_reaches_the_sink_after_finish_returns() {
        let registry = ReporterRegistry::new();
        let (parked, sink) = parking();
        let reporter = registry
            .install(Reporter::new(HANDOFF, sink))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        let producer = std::thread::spawn(move || {
            assert!(matches!(
                pushing.push(None, report("shizuku.udp_send", 42)),
                Pushed::Coalesced
            ));
            pushing
        });
        parked
            .entered
            .recv()
            .expect("the producer reaches the sink");

        let mut finishing = Box::pin(reporter.finish());
        tokio::time::timeout(WAIT, &mut finishing)
            .await
            .expect_err("a finish must not return while a producer is still emitting");
        assert_eq!(parked.late.load(Ordering::SeqCst), 0);

        parked.release.send(()).expect("the producer is waiting");
        finishing.await.expect("the flush must complete");
        parked.finished.store(true, Ordering::SeqCst);
        let pushing = producer.join().expect("the producer completes");
        assert!(matches!(
            pushing.push(None, report("shizuku.udp_send", 42)),
            Pushed::Closed(_)
        ));
        assert_eq!(count(&parked.emitted), 1);
        assert_eq!(parked.late.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_successor_cannot_install_until_the_predecessor_has_finished() {
        let registry = ReporterRegistry::new();
        let (parked, sink) = parking();
        let reporter = registry
            .install(Reporter::new(HANDOFF, sink))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        let producer = std::thread::spawn(move || {
            pushing.push(None, report("shizuku.tun_egress", 99));
        });
        parked
            .entered
            .recv()
            .expect("the producer reaches the sink");

        let mut finishing = Box::pin(reporter.finish());
        tokio::time::timeout(WAIT, &mut finishing)
            .await
            .expect_err("a finish must not return while a producer is still emitting");
        assert!(registry.get().is_none());
        let (early_emitted, early_sink) = collecting();
        let refused = match registry.install(Reporter::new(HANDOFF, early_sink)) {
            Ok(_) => panic!("a successor must not install while a finish is still running"),
            Err(e) => e,
        };
        assert!(refused.to_string().contains("already installed"));
        assert_eq!(Arc::strong_count(&early_emitted), 1);

        parked.release.send(()).expect("the producer is waiting");
        finishing.await.expect("the flush must complete");
        producer.join().expect("the producer completes");
        let (emitted, sink) = collecting();
        let successor = registry
            .install(Reporter::new(HANDOFF, sink))
            .expect("a completed finish releases the registration");
        registry
            .get()
            .expect("the successor is the process's reporter")
            .push(None, report("shizuku.echo_socket", 12));
        assert_eq!(count(&emitted), 1);
        successor.finish().await.expect("the flush must complete");
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_finish_leaves_the_same_finalizer_running() {
        let registry = ReporterRegistry::new();
        let (parked, sink) = parking();
        let reporter = registry
            .install(Reporter::new(HANDOFF, sink))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        let watching = Arc::clone(&pushing);
        let producer = std::thread::spawn(move || {
            pushing.push(None, report("shizuku.udp_send", 42));
        });
        parked
            .entered
            .recv()
            .expect("the producer reaches the sink");

        {
            let mut finishing = Box::pin(reporter.finish());
            tokio::time::timeout(WAIT, &mut finishing)
                .await
                .expect_err("the drain has not completed");
        }
        let (_, early_sink) = collecting();
        assert!(registry
            .install(Reporter::new(HANDOFF, early_sink))
            .is_err());

        parked.release.send(()).expect("the producer is waiting");
        producer.join().expect("the producer completes");
        watching
            .terminal()
            .await
            .expect("the detached finalizer completes");
        let (emitted, sink) = collecting();
        let successor = registry
            .install(Reporter::new(HANDOFF, sink))
            .expect("a completed finalizer releases the registration");
        registry
            .get()
            .expect("the successor is the process's reporter")
            .push(None, report("shizuku.echo_socket", 12));
        assert_eq!(count(&emitted), 1);
        assert_eq!(count(&parked.emitted), 1);
        successor.finish().await.expect("the flush must complete");
    }

    #[tokio::test(start_paused = true)]
    async fn a_finish_dropped_at_the_final_flush_leaves_the_same_finalizer_running() {
        let registry = ReporterRegistry::new();
        let held: Arc<Mutex<Vec<(NonfatalReport, Handed)>>> = Arc::new(Mutex::new(Vec::new()));
        let arrived = Arc::new(Notify::new());
        let sink = Arc::clone(&held);
        let sink_arrived = Arc::clone(&arrived);
        let reporter = registry
            .install(Reporter::new(1, move |report, permit| {
                sink.lock()
                    .expect("the sink is poisoned")
                    .push((report, permit));
                sink_arrived.notify_one();
                true
            }))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        for _ in 0..2 {
            assert!(matches!(
                pushing.push(None, report("shizuku.udp_send", 42)),
                Pushed::Coalesced
            ));
        }
        assert_eq!(held.lock().expect("the sink is poisoned").len(), 1);
        arrived.notified().await;

        {
            let mut finishing = Box::pin(reporter.finish());
            tokio::time::timeout(WAIT, &mut finishing)
                .await
                .expect_err("the flush has nowhere to hand its summary");
        }
        let (_, early_sink) = collecting();
        assert!(registry
            .install(Reporter::new(HANDOFF, early_sink))
            .is_err());

        held.lock().expect("the sink is poisoned").remove(0);
        arrived.notified().await;
        assert_eq!(held.lock().expect("the sink is poisoned").len(), 1);
        held.lock().expect("the sink is poisoned").clear();
        pushing
            .terminal()
            .await
            .expect("the detached finalizer completes");
        let (emitted, sink) = collecting();
        let successor = registry
            .install(Reporter::new(HANDOFF, sink))
            .expect("a completed finalizer releases the registration");
        registry
            .get()
            .expect("the successor is the process's reporter")
            .push(None, report("shizuku.echo_socket", 12));
        assert_eq!(count(&emitted), 1);
        successor.finish().await.expect("the flush must complete");
    }

    #[tokio::test]
    async fn a_blocked_handoff_retains_one_latest_summary_per_source() {
        let registry = ReporterRegistry::new();
        let held: Arc<Mutex<Vec<(NonfatalReport, Handed)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&held);
        let reporter = registry
            .install(Reporter::new(2, move |report, permit| {
                sink.lock()
                    .expect("the sink is poisoned")
                    .push((report, permit));
                true
            }))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        let sites = [42u32, 77u32];
        for line in sites {
            assert!(matches!(
                pushing.push(None, report("shizuku.udp_send", line)),
                Pushed::Coalesced
            ));
        }
        assert_eq!(held.lock().expect("the sink is poisoned").len(), 2);
        for _ in 0..50u32 {
            for line in sites {
                assert!(matches!(
                    pushing.push(None, report("shizuku.udp_send", line)),
                    Pushed::Coalesced
                ));
            }
        }
        assert_eq!(held.lock().expect("the sink is poisoned").len(), 2);

        held.lock().expect("the sink is poisoned").clear();
        reporter.finish().await.expect("the flush must complete");
        let held = held.lock().expect("the sink is poisoned");
        assert_eq!(held.len(), 2);
        for (report, _) in held.iter() {
            assert!(sites.contains(&report.report.line));
            assert_eq!(suppressed(report), Some("49"));
        }
    }

    #[tokio::test]
    async fn freeing_the_one_place_drains_every_waiting_site_in_order() {
        let registry = ReporterRegistry::new();
        let places: Arc<Mutex<Vec<Handed>>> = Arc::new(Mutex::new(Vec::new()));
        let (emitted, mut arriving) = tokio::sync::mpsc::unbounded_channel();
        let sink = Arc::clone(&places);
        let reporter = registry
            .install(Reporter::new(1, move |report, place| {
                sink.lock().expect("the sink is poisoned").push(place);
                emitted.send(report).is_ok()
            }))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        let sites = [42u32, 77u32, 99u32];
        for line in sites {
            for _ in 0..2 {
                assert!(matches!(
                    pushing.push(None, report("shizuku.udp_send", line)),
                    Pushed::Coalesced
                ));
            }
        }
        let first = arriving.recv().await.expect("the first site is emitted");
        assert!(sites.contains(&first.report.line));
        assert_eq!(suppressed(&first), None);
        assert_eq!(places.lock().expect("the sink is poisoned").len(), 1);
        assert!(arriving.try_recv().is_err());

        assert!(
            arriving.try_recv().is_err(),
            "an occupied handoff keeps every source pending",
        );

        let mut summarised = Vec::new();
        for _ in 0..sites.len() {
            places.lock().expect("the sink is poisoned").remove(0);
            let summary = arriving
                .recv()
                .await
                .expect("a place coming back releases a waiting summary");
            summarised.push(summary.report.line);
        }
        summarised.sort_unstable();
        assert_eq!(
            summarised, sites,
            "every site drains, so the one that won the place cannot starve the others",
        );
        reporter.finish().await.expect("the flush must complete");
    }

    #[tokio::test(start_paused = true)]
    async fn the_final_flush_waits_for_the_writer_between_summaries() {
        let registry = ReporterRegistry::new();
        let held: Arc<Mutex<Vec<(NonfatalReport, Handed)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&held);
        let reporter = registry
            .install(Reporter::new(1, move |report, permit| {
                sink.lock()
                    .expect("the sink is poisoned")
                    .push((report, permit));
                true
            }))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        for line in [42u32, 77u32] {
            for _ in 0..2 {
                assert!(matches!(
                    pushing.push(None, report("shizuku.udp_send", line)),
                    Pushed::Coalesced
                ));
            }
        }
        let taken = |held: &Arc<Mutex<Vec<(NonfatalReport, Handed)>>>| {
            held.lock().expect("the sink is poisoned").len()
        };
        assert_eq!(taken(&held), 1);

        let mut finishing = Box::pin(reporter.finish());
        tokio::time::timeout(WAIT, &mut finishing)
            .await
            .expect_err("the flush must wait for the one permit");
        assert_eq!(taken(&held), 1);

        held.lock().expect("the sink is poisoned").remove(0);
        tokio::time::timeout(WAIT, &mut finishing)
            .await
            .expect_err("the flush must wait between summaries");
        assert_eq!(taken(&held), 1);

        held.lock().expect("the sink is poisoned").remove(0);
        finishing.await.expect("the flush must complete");
        assert_eq!(taken(&held), 1);
        held.lock().expect("the sink is poisoned").remove(0);
        assert_eq!(taken(&held), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_writer_that_disconnects_reports_what_it_could_not_take_at_finish() {
        let registry = ReporterRegistry::new();
        let held: Arc<Mutex<Vec<(NonfatalReport, Handed)>>> = Arc::new(Mutex::new(Vec::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let sink = Arc::clone(&held);
        let sink_connected = Arc::clone(&connected);
        let reporter = registry
            .install(Reporter::new(1, move |report, permit| {
                if !sink_connected.load(Ordering::SeqCst) {
                    return false;
                }
                sink.lock()
                    .expect("the sink is poisoned")
                    .push((report, permit));
                true
            }))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        for line in [42u32, 77u32] {
            for _ in 0..2 {
                assert!(matches!(
                    pushing.push(None, report("shizuku.udp_send", line)),
                    Pushed::Coalesced
                ));
            }
        }
        assert_eq!(held.lock().expect("the sink is poisoned").len(), 1);

        let mut finishing = Box::pin(reporter.finish());
        tokio::time::timeout(WAIT, &mut finishing)
            .await
            .expect_err("the flush must wait for the one permit");
        connected.store(false, Ordering::SeqCst);
        held.lock().expect("the sink is poisoned").clear();
        let failed = finishing.await.expect_err(
            "a report the writer could not take has to reach the conversation's result",
        );
        assert!(failed.to_string().contains("could not be delivered"));
    }
}
