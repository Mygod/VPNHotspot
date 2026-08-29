//! Coalesces nonfatal reports before handing them to the session writer.
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::shared::nonfatal::{NonfatalReport, SiteCoalescer};
use crate::shared::proto::daemon::DaemonErrorReport;

/// Where an emitted report goes, and whether it got there. `false` is a report the conversation could no
/// longer carry, which the finalizer turns into the session's own failure rather than losing.
type Sink = Box<dyn Fn(NonfatalReport, Handed) -> bool + Send + Sync>;

/// A reporter's share of its conversation's writer queue: how many reports may be waiting in it at once.
struct Handoff {
    available: AtomicUsize,
    /// Woken when a place comes back, which is the only thing that can un-stick a window the handoff had no
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
    /// Gives the place back and announces it in the same step, so the window that had no room for a summary is
    /// woken by the writer's own progress rather than by a timer.
    fn drop(&mut self) {
        self.0.available.fetch_add(1, Ordering::AcqRel);
        self.0.freed.notify_one();
    }
}

/// The clock the windows are kept on.
fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// What became of one report, which is a thing the caller has to be able to act on rather than assume.
pub enum Pushed {
    /// Taken: coalesced, and emitted now or when its window closes.
    Coalesced,
    /// Handed back, because the conversation that owned the reporter has finished. Nothing was coalesced,
    /// emitted or opened - so this is not a report in flight, it is a report with nowhere to go, and the
    /// caller is the one that knows where to write it.
    Closed(DaemonErrorReport),
}

pub struct Reporter {
    state: Arc<State>,
    cancel: CancellationToken,
    /// Everything about ending this reporter, under one lock: the window task, whether a finalizer has been
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
    /// dropped future cannot detach the task the session's result depends on.
    window: Option<JoinHandle<()>>,
    /// Whether a finalizer task has been spawned. Exactly one ever is.
    running: bool,
    /// What that finalizer concluded, or `None` while it has not concluded anything. A message rather than an
    /// `io::Error` because every waiter gets the same answer and `io::Error` is not cloneable.
    outcome: Option<Result<(), String>>,
}

struct State {
    /// The coalescer and whether it still admits reports, under one lock because those two have to be
    /// checked and acted on together: a report admitted after the final flush would sit in a window nothing
    /// will ever close, which is exactly the allocation a finished reporter must not make.
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
    /// Woken by a push that opened a window, which is the only thing the task below waits for besides the
    /// window itself.
    opened: Notify,
}

struct Admission {
    coalescer: SiteCoalescer,
    closed: bool,
}

impl Reporter {
    /// Builds a reporter that holds no task yet.
    pub fn new(
        window: Duration,
        handoff: usize,
        sink: impl Fn(NonfatalReport, Handed) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            state: Arc::new(State {
                admission: Mutex::new(Admission {
                    coalescer: SiteCoalescer::new(window),
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

    /// Coalesces one report and emits whatever that made due. Never waits, so a packet path can call it.
    pub fn push(&self, call_id: Option<u64>, report: DaemonErrorReport) -> Pushed {
        let mut refused = None;
        self.state.emit(|admission, room| {
            if admission.closed {
                refused = Some(report);
                return Vec::new();
            }
            admission.coalescer.push(now(), call_id, report, room)
        });
        match refused {
            Some(report) => Pushed::Closed(report),
            None => {
                // A first report of its kind opens a window whose summary falls due later, and nothing else
                // in the process would wake for it.
                self.state.opened.notify_one();
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

    /// Starts the window task. Called by [ReporterRegistry::install] once this reporter is where producers
    /// can find it, which is the only order in which a failed installation spawns nothing.
    fn open(&self) {
        let task = tokio::spawn(close_windows(Arc::clone(&self.state), self.cancel.clone()));
        let previous = self.locked_shutdown().window.replace(task);
        debug_assert!(previous.is_none());
    }
}

impl State {
    /// The window's lock, held for the coalescer's own bookkeeping and for taking places in the writer's
    /// queue, and never across an await. Poisoning cannot happen: the daemon aborts on panic.
    fn locked(&self) -> MutexGuard<'_, Admission> {
        self.admission
            .lock()
            .expect("the report coalescer is poisoned")
    }

    /// One emission: `due` decides what to hand over, under the admission lock and against the room actually
    /// available, and the sink is then called outside it.
    fn emit(&self, due: impl FnOnce(&mut Admission, usize) -> Vec<NonfatalReport>) {
        let taken = {
            let mut admission = self.locked();
            let reports = due(&mut admission, self.handoff.available());
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
            // the window task cancelled so that nothing accumulates or wakes for a window nobody will close;
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
    let window = shutdown.window.take();
    drop(shutdown);
    let registry = Arc::clone(registry);
    let reporter = Arc::clone(reporter);
    runtime.spawn(finalize(registry, reporter, window));
}

/// Ends one conversation's reporting, in the one order that makes each step's promise true of the next.
async fn finalize(
    registry: Arc<RegistryInner>,
    reporter: Arc<Reporter>,
    window: Option<JoinHandle<()>>,
) {
    let last = {
        let mut admission = reporter.state.locked();
        admission.closed = true;
        admission.coalescer.flush()
    };
    reporter.cancel.cancel();
    reporter.state.drain().await;
    let mut failure = match window {
        Some(window) => window
            .await
            .err()
            .map(|e| format!("the report window task failed: {e}")),
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

/// Emits each window's summary when it falls due. The only part of reporting that needs a task at all.
async fn close_windows(state: Arc<State>, cancel: CancellationToken) {
    loop {
        // Ahead of the wait rather than inside one of its arms, because what to wait for is decided from what
        // is left afterwards - and because a window already due must be closed before another push is taken
        // into account: a steady stream of reports must not keep postponing the summary it is producing.
        state.emit(|admission, room| admission.coalescer.emit_due(now(), room));
        let deadline = state.locked().coalescer.next_deadline();
        // A window still due after that is one the handoff had no room for, so what this waits for is room
        // rather than time. Sleeping until a deadline already in the past would spin instead.
        let full = deadline.is_some_and(|deadline| deadline <= now());
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            // A wakeup and nothing more - the emission at the top of the loop is what takes the place it
            // needs. Enabled only while something is waiting on room, so an idle daemon does not spin here.
            () = state.handoff.freed.notified(), if full => {}
            () = due(deadline), if !full => {}
            () = state.opened.notified() => {}
        }
    }
}

/// Waits for one window to fall due, or forever when none is open. `pending` rather than an interval, so an
/// idle daemon costs no wakeups.
async fn due(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    use super::*;

    const WINDOW: Duration = Duration::from_secs(1);
    const HANDOFF: usize = 8;

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

    fn suppressed(report: &NonfatalReport) -> Option<&str> {
        report
            .report
            .details
            .iter()
            .find(|detail| detail.key == "coalesced.suppressed_count")
            .map(|detail| detail.value.as_str())
    }

    #[tokio::test]
    async fn a_flood_is_coalesced_before_anything_is_queued() {
        let registry = ReporterRegistry::new();
        let (emitted, sink) = collecting();
        let reporter = registry
            .install(Reporter::new(Duration::from_secs(60), HANDOFF, sink))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        for _ in 0..10_000 {
            assert!(matches!(
                pushing.push(None, report("shizuku.udp_send", 42)),
                Pushed::Coalesced
            ));
        }
        assert_eq!(count(&emitted), 1);
        drop(pushing);
        reporter.finish().await.expect("the flush must complete");
        let emitted = emitted.lock().expect("the sink is poisoned");
        assert_eq!(emitted.len(), 2);
        assert_eq!(suppressed(&emitted[1]), Some("9999"));
    }

    #[tokio::test(start_paused = true)]
    async fn the_window_task_closes_a_window_and_finish_joins_it() {
        let registry = ReporterRegistry::new();
        let (emitted, sink) = collecting();
        let reporter = registry
            .install(Reporter::new(WINDOW, HANDOFF, sink))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        for call_id in [7, 8] {
            assert!(matches!(
                pushing.push(Some(call_id), report("shizuku.echo_send", 11)),
                Pushed::Coalesced
            ));
        }
        assert_eq!(count(&emitted), 1);
        tokio::time::sleep(WINDOW * 2).await;
        {
            let emitted = emitted.lock().expect("the sink is poisoned");
            assert_eq!(emitted.len(), 2);
            assert_eq!(emitted[1].call_id, Some(8));
            assert_eq!(suppressed(&emitted[1]), Some("1"));
        }
        reporter.finish().await.expect("the flush must complete");
        assert!(matches!(
            pushing.push(None, report("shizuku.echo_send", 11)),
            Pushed::Closed(_)
        ));
        tokio::time::sleep(WINDOW * 2).await;
        assert_eq!(count(&emitted), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_window_is_flushed_exactly_once_when_the_conversation_finishes() {
        let registry = ReporterRegistry::new();
        let (emitted, sink) = collecting();
        let reporter = registry
            .install(Reporter::new(WINDOW, HANDOFF, sink))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        for _ in 0..2 {
            assert!(matches!(
                pushing.push(None, report("shizuku.tun_output", 5)),
                Pushed::Coalesced
            ));
        }
        assert_eq!(count(&emitted), 1);
        tokio::time::advance(WINDOW * 2).await;
        reporter.finish().await.expect("the flush must complete");
        let emitted = emitted.lock().expect("the sink is poisoned");
        assert_eq!(emitted.len(), 2);
        assert_eq!(suppressed(&emitted[1]), Some("1"));
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_reported_after_the_conversation_finished() {
        let registry = ReporterRegistry::new();
        let (emitted, sink) = collecting();
        let reporter = registry
            .install(Reporter::new(WINDOW, HANDOFF, sink))
            .expect("the first installation must be accepted");
        let racing = registry.get().expect("a producer finds the reporter");
        reporter.finish().await.expect("the flush must complete");
        assert!(registry.get().is_none());
        assert!(matches!(
            racing.push(Some(3), report("shizuku.udp_send", 42)),
            Pushed::Closed(_)
        ));
        tokio::time::sleep(WINDOW * 3).await;
        assert_eq!(count(&emitted), 0);
        drop(racing);
        assert_eq!(count(&emitted), 0);
    }

    #[tokio::test]
    async fn a_second_installation_is_refused_and_leaves_nothing_running() {
        let registry = ReporterRegistry::new();
        let (emitted, sink) = collecting();
        let reporter = registry
            .install(Reporter::new(WINDOW, HANDOFF, sink))
            .expect("the first installation must be accepted");
        let (refused_emitted, refused_sink) = collecting();
        let refused = match registry.install(Reporter::new(WINDOW, HANDOFF, refused_sink)) {
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
            .install(Reporter::new(WINDOW, HANDOFF, successor_sink))
            .expect("a finished conversation releases the registration");
        registry
            .get()
            .expect("the successor is the process's reporter")
            .push(None, report("shizuku.tcp_sweep", 7));
        assert_eq!(count(&successor_emitted), 1);
        successor.finish().await.expect("the flush must complete");
    }

    #[tokio::test(start_paused = true)]
    async fn a_guard_dropped_without_finishing_ends_the_reporter() {
        let registry = ReporterRegistry::new();
        let (abandoned_emitted, sink) = collecting();
        let abandoned = {
            let _guard = registry
                .install(Reporter::new(WINDOW, HANDOFF, sink))
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
        tokio::time::sleep(WINDOW * 3).await;
        assert!(matches!(
            abandoned.push(None, report("shizuku.tun_output", 5)),
            Pushed::Closed(_)
        ));
        assert_eq!(count(&abandoned_emitted), 2);
        drop(abandoned);
        let (emitted, sink) = collecting();
        let successor = registry
            .install(Reporter::new(WINDOW, HANDOFF, sink))
            .expect("an abandoned registration must not outlive its conversation");
        registry
            .get()
            .expect("the successor is the process's reporter")
            .push(None, report("shizuku.echo_socket", 12));
        assert_eq!(count(&emitted), 1);
        successor.finish().await.expect("the flush must complete");
    }

    #[tokio::test]
    async fn a_report_the_conversation_could_not_carry_fails_the_session() {
        let registry = ReporterRegistry::new();
        let reporter = registry
            .install(Reporter::new(WINDOW, HANDOFF, |_: NonfatalReport, _| false))
            .expect("the first installation must be accepted");
        registry
            .get()
            .expect("a producer finds the reporter")
            .push(None, report("shizuku.tun_egress", 99));
        let failed = reporter
            .finish()
            .await
            .expect_err("an undelivered report has to reach the session's result");
        assert!(failed.to_string().contains("could not be delivered"));
    }

    #[test]
    fn a_guard_dropped_outside_a_runtime_leaves_the_registry_fail_closed() {
        let runtime = tokio::runtime::Runtime::new().expect("a runtime for the installation");
        let registry = ReporterRegistry::new();
        let (emitted, sink) = collecting();
        let guard = runtime.enter();
        let reporter = registry
            .install(Reporter::new(WINDOW, HANDOFF, sink))
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
            .install(Reporter::new(WINDOW, HANDOFF, successor_sink))
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
            .install(Reporter::new(WINDOW, HANDOFF, sink))
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
        tokio::time::timeout(WINDOW * 4, &mut finishing)
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
            .install(Reporter::new(WINDOW, HANDOFF, sink))
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
        tokio::time::timeout(WINDOW * 4, &mut finishing)
            .await
            .expect_err("a finish must not return while a producer is still emitting");
        assert!(registry.get().is_none());
        let (early_emitted, early_sink) = collecting();
        let refused = match registry.install(Reporter::new(WINDOW, HANDOFF, early_sink)) {
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
            .install(Reporter::new(WINDOW, HANDOFF, sink))
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
            .install(Reporter::new(WINDOW, HANDOFF, sink))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        let producer = std::thread::spawn(move || {
            pushing.push(None, report("shizuku.udp_send", 42));
        });
        parked
            .entered
            .recv()
            .expect("the producer reaches the sink");

        {
            let mut finishing = Box::pin(reporter.finish());
            tokio::time::timeout(WINDOW * 4, &mut finishing)
                .await
                .expect_err("the drain has not completed");
        }
        let (_, early_sink) = collecting();
        assert!(registry
            .install(Reporter::new(WINDOW, HANDOFF, early_sink))
            .is_err());

        parked.release.send(()).expect("the producer is waiting");
        producer.join().expect("the producer completes");
        tokio::time::sleep(WINDOW * 4).await;
        let (emitted, sink) = collecting();
        let successor = registry
            .install(Reporter::new(WINDOW, HANDOFF, sink))
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
        let sink = Arc::clone(&held);
        let reporter = registry
            .install(Reporter::new(WINDOW, 1, move |report, permit| {
                sink.lock()
                    .expect("the sink is poisoned")
                    .push((report, permit));
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

        {
            let mut finishing = Box::pin(reporter.finish());
            tokio::time::timeout(WINDOW * 4, &mut finishing)
                .await
                .expect_err("the flush has nowhere to hand its summary");
        }
        let (_, early_sink) = collecting();
        assert!(registry
            .install(Reporter::new(WINDOW, HANDOFF, early_sink))
            .is_err());

        held.lock().expect("the sink is poisoned").remove(0);
        tokio::time::sleep(WINDOW * 4).await;
        assert_eq!(held.lock().expect("the sink is poisoned").len(), 1);
        held.lock().expect("the sink is poisoned").clear();
        let (emitted, sink) = collecting();
        let successor = registry
            .install(Reporter::new(WINDOW, HANDOFF, sink))
            .expect("a completed finalizer releases the registration");
        registry
            .get()
            .expect("the successor is the process's reporter")
            .push(None, report("shizuku.echo_socket", 12));
        assert_eq!(count(&emitted), 1);
        successor.finish().await.expect("the flush must complete");
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_handoff_holds_reports_in_their_windows_across_windows() {
        let registry = ReporterRegistry::new();
        let held: Arc<Mutex<Vec<(NonfatalReport, Handed)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&held);
        let reporter = registry
            .install(Reporter::new(WINDOW, 2, move |report, permit| {
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
        for _ in 0..5u32 {
            for _ in 0..10u32 {
                for line in sites {
                    assert!(matches!(
                        pushing.push(None, report("shizuku.udp_send", line)),
                        Pushed::Coalesced
                    ));
                }
            }
            tokio::time::advance(WINDOW * 2).await;
        }
        assert_eq!(held.lock().expect("the sink is poisoned").len(), 2);

        held.lock().expect("the sink is poisoned").clear();
        reporter.finish().await.expect("the flush must complete");
        let held = held.lock().expect("the sink is poisoned");
        assert_eq!(held.len(), 2);
        for (report, _) in held.iter() {
            assert!(sites.contains(&report.report.line));
            assert_eq!(suppressed(report), Some("50"));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_final_flush_waits_for_the_writer_between_summaries() {
        let registry = ReporterRegistry::new();
        let held: Arc<Mutex<Vec<(NonfatalReport, Handed)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&held);
        let reporter = registry
            .install(Reporter::new(WINDOW, 1, move |report, permit| {
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
        tokio::time::timeout(WINDOW * 4, &mut finishing)
            .await
            .expect_err("the flush must wait for the one permit");
        assert_eq!(taken(&held), 1);

        held.lock().expect("the sink is poisoned").remove(0);
        tokio::time::timeout(WINDOW * 4, &mut finishing)
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
    async fn a_writer_that_disconnects_fails_the_reports_it_could_not_take() {
        let registry = ReporterRegistry::new();
        let held: Arc<Mutex<Vec<(NonfatalReport, Handed)>>> = Arc::new(Mutex::new(Vec::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let sink = Arc::clone(&held);
        let sink_connected = Arc::clone(&connected);
        let reporter = registry
            .install(Reporter::new(WINDOW, 1, move |report, permit| {
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
        tokio::time::timeout(WINDOW * 4, &mut finishing)
            .await
            .expect_err("the flush must wait for the one permit");
        connected.store(false, Ordering::SeqCst);
        held.lock().expect("the sink is poisoned").clear();
        let failed = finishing
            .await
            .expect_err("a report the writer could not take has to reach the session's result");
        assert!(failed.to_string().contains("could not be delivered"));
    }
}
