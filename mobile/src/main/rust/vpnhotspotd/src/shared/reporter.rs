//! The nonfatal reporter's ownership: coalescing before the queue, a window task its session joins, and a
//! registration that cannot outlive either.
//!
//! Two properties matter here and they pull in opposite directions. A report may be made from anywhere,
//! including the packet paths, so producing one must not await anything - and the input that drives those
//! paths is attacker-influenced, so a queue in front of the coalescer is a hole: one forged packet per
//! report, each cloning a `DaemonErrorReport` into a queue nobody is draining, is memory a client chooses.
//!
//! So the coalescer is on the producer's side of the queue rather than behind it. [Reporter::push] takes the
//! window's lock, coalesces, and hands whatever is due straight to the sink, which bounds what exists at any
//! moment to one pending batch per distinct report site - a count fixed by the source, not by traffic. What
//! remains for a task is only the one thing a producer cannot do: close a window that nothing else will wake
//! for. That task is owned, cancelled and joined by the finalizer below, so a session that has finished
//! reporting has provably stopped reporting.
//!
//! Coalescing bounds what a *client* can make this hold; the handoff is what bounds what a slow *controller*
//! can. The sink puts a report on the control writer's queue, which is unbounded because a frame the session
//! must not lose can always be put on it - so if reporting kept feeding it while nobody drained it, one
//! summary per window per site would accumulate for as long as the daemon ran. [Reporter] therefore owns a
//! fixed number of places in that queue, hands each report one, and gets it back when the writer has dropped
//! the message it wrote. A report that finds none is not dropped: it stays in its window as one more
//! occurrence, and the window closes when a place comes free. Nothing polls for that - the permit's own
//! release is the wakeup.
//!
//! The reporter is nevertheless reachable from every packet path, and [ReporterRegistry] is how - *weakly*,
//! which is the second half of the ownership. A strong global would keep the reporter, and through its sink
//! the control writer's sender, alive past the point the conversation that built it dropped it, and the
//! session would never see the writer's own result. It is also what makes "finished" real: an invalidated
//! registration cannot be upgraded, so a report made afterwards has nothing to revive.
//!
//! # What ends a conversation's reporting
//!
//! Not a `Drop`, and that is deliberate. Closing admission, draining the producers already past it, joining
//! the window task and flushing what the coalescer still holds are all things that have to *wait* for
//! something, and a `Drop` cannot wait. So there is exactly one finalizer, it is a task, and both
//! [ReporterGuard::finish] and [ReporterGuard]'s own `Drop` do the same thing: make sure it has been started.
//! `finish` additionally awaits its outcome; `Drop` does not, and does not pretend to have cleaned anything
//! up synchronously.
//!
//! The registration stays in [Registration::closing] for the whole of that, so no successor can install into
//! the window where the predecessor's producers are still emitting and its task is still being joined. Only
//! the finalizer that actually ran releases it, and only for the exact reporter it was started for.
//!
//! One consequence is worth stating plainly: a controller that has stopped reading and never disconnects can
//! keep a finalization - and therefore the registry - closed forever. That is the honest outcome. The
//! alternative is a timeout that declares a report delivered when it is still in a queue nobody is draining.

mod coalescer;

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::shared::nonfatal::NonfatalReport;
use crate::shared::proto::daemon::DaemonErrorReport;
use coalescer::SiteCoalescer;

/// Where an emitted report goes, and whether it got there. `false` is a report the conversation could no
/// longer carry, which the finalizer turns into the session's own failure rather than losing.
///
/// The [Handed] place travels *with* the report: whatever the sink hands it to owns that place until the
/// message has been written and dropped, and dropping it is what gives the place back. Handing it over rather
/// than releasing it here is the difference between a bound on what is waiting to be written and a bound on
/// nothing at all.
///
/// Note what this does *not* prove. A sink that returns `true` has put the frame on the writer's queue; that
/// the bytes reached the peer is proven by joining the writer task, which is the session's own step and not
/// this one's.
type Sink = Box<dyn Fn(NonfatalReport, Handed) -> bool + Send + Sync>;

/// A reporter's share of its conversation's writer queue: how many reports may be waiting in it at once.
///
/// Counted here rather than taken from a semaphore because the count has to be *exact* under the admission
/// lock. A waiter queued on a semaphore is assigned the next place that comes free and keeps it until it is
/// polled, so a reporter asking how much room it has would see one fewer than there is and call a report
/// undeliverable that the writer had already made room for.
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

    /// How many places are free. Read under the admission lock, where it is a floor rather than a guess:
    /// every place a *producer* takes is taken under that lock, so nothing else can spend what this promises
    /// and a release only ever adds to it. The finalizer takes places outside that lock, and may, because by
    /// then admission is closed, every producer has drained and the window task has been joined - so it is
    /// the only thing left that emits at all.
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
    ///
    /// The notification is registered *before* the recheck, so a release landing between the two wakes this
    /// rather than being lost.
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
///
/// Tokio's rather than the process's, and they differ in exactly one place: a test that pauses time moves
/// this one and not the other. [due] sleeps on tokio's clock, so a coalescer keeping its deadlines on the
/// process clock would be a window that falls due at one time and is noticed at another - a wait that never
/// ends where time is paused, and a busy loop where it is only slow.
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
    /// How many producers have left the admission lock with reports to hand over and have not finished doing
    /// so. Incremented under that lock, so a producer is either counted before the finalizer closes admission
    /// or refused by it - never in between, which is the gap a finalization would otherwise return in the
    /// middle of.
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
    ///
    /// Nothing is spawned here on purpose: [ReporterRegistry::install] starts the window task, and only
    /// after the registration has succeeded, so a refused installation cannot leave one behind.
    ///
    /// `handoff` is how many reports may be waiting in the conversation's writer queue at once. It bounds
    /// what a controller that has stopped reading can make this reporter accumulate downstream; the
    /// coalescer bounds what a client can make it accumulate here.
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
    ///
    /// A report made after the owning conversation began finishing is handed straight back instead: nothing is
    /// coalesced, nothing is emitted, and no window is opened, because there is no longer a task that would
    /// close one. Returning it rather than logging it here is what keeps the working path free of a clone -
    /// the caller is the one that knows where a report with nowhere to go should be written.
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
        debug_assert!(previous.is_none(), "the reporter was opened twice");
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
    ///
    /// Both halves are needed where they are. Deciding under the lock is what makes the places taken below
    /// infallible and what makes this emission visible to a concurrent finalization before it can return.
    /// Calling the sink outside it is what keeps a producer off the critical path of every other producer:
    /// the sink encodes a frame and hands it to another task, and these calls come from the packet paths.
    ///
    /// A sink must not report anything of its own. It runs while this producer is counted as emitting, so a
    /// report made from inside one would deadlock rather than be carried.
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
    ///
    /// The count is re-read around the wait rather than trusted once: [Notify::notify_one] leaves a permit
    /// behind when nobody is waiting yet, so the producer that finishes just before this is awaited wakes it
    /// anyway, and a spurious wake merely re-reads a count that is already zero.
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
///
/// The registration is a [Weak], so being reachable is not being owned: see the module note. One
/// installation may be live at a time, which is what keeps two overlapping conversations from each believing
/// they own reporting.
///
/// Held behind an [Arc] because the finalizer outlives the guard that started it and has to be able to
/// release the registration when it is done. A production instance is a `LazyLock` static; a test builds one
/// per test.
#[derive(Clone)]
pub struct ReporterRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    current: Mutex<Registration>,
}

/// What the process knows about reporting: who to find, and who is still on their way out.
///
/// The two are separate because they end at different moments. A producer must stop finding the reporter the
/// instant its conversation begins finishing - upgrading one that is tearing down is the race - while a
/// *successor* has to wait for the whole of that finish: the predecessor's producers are still being drained,
/// its window task is still being joined, and its last summaries are still being handed to a writer. Two
/// conversations each believing they own reporting is the one thing this registry exists to prevent.
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
    ///
    /// The order is the whole reason this is a method rather than two calls: the registration happens first,
    /// so a refusal returns having spawned nothing, and the window task is started immediately afterwards,
    /// so nothing can find a reporter whose windows nobody would close.
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
    ///
    /// Consumes the guard, because finishing twice is not a thing a conversation does. Dropping this future
    /// part way through does not undo anything and does not stop anything: the finalizer is a task, so what a
    /// dropped waiter gives up is the answer, not the work.
    pub async fn finish(self) -> io::Result<()> {
        ensure_finalized(&self.registry, &self.reporter);
        self.reporter.terminal().await
    }
}

impl Drop for ReporterGuard {
    /// Makes sure the same finalizer has been started, and nothing else.
    ///
    /// Deliberately not a cleanup. Closing admission, draining producers, joining the window task and
    /// handing the last summaries to a writer all have to wait for something, and a `Drop` cannot - so
    /// claiming any of it had happened by the time this returns would be a lie. What this guarantees is that
    /// it *will* happen, and that the registration stays busy until it has.
    fn drop(&mut self) {
        ensure_finalized(&self.registry, &self.reporter);
    }
}

/// Starts the one finalizer for this reporter, if it has not been started already.
///
/// Called by both [ReporterGuard::finish] and [ReporterGuard]'s `Drop`, and idempotent between them: the
/// second caller finds it running and leaves it alone. The registration is marked as closing here rather
/// than inside the task, so no successor can install in the gap between the guard giving up its reference
/// and the finalizer getting its first poll.
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
///
/// Closing admission first is what makes the extracted flush the last word: nothing can enter a window after
/// it, so no report is left in one nothing will close. Extracting under the same lock is what keeps a pending
/// report from waiting on a task that has not woken yet, and what keeps the two from racing for it. Draining
/// the in-flight producers is what makes the promise true of *them*: a producer that was past the closed flag
/// when it was set is still on its way to the sink, and this is the only thing that knows about it. Joining
/// the task covers the one emitter that is not a producer.
///
/// Only then are the last summaries handed over, one at a time, because by that point this is the only thing
/// left that emits at all - which is what makes it safe to take places in the writer's queue outside the
/// admission lock every other acquisition is made under.
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
    /// Room enough that these tests never reach it, except the ones that are about reaching it.
    const HANDOFF: usize = 8;

    /// Reports are keyed by source site, so a test that wants two distinct batches varies the line.
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

    /// A sink that behaves like a controller keeping up: the place in the writer's queue goes straight back,
    /// as it does when the frame has been written and its message dropped.
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
        // What a client can drive: one report per forged packet, from one site.
        for _ in 0..10_000 {
            assert!(matches!(
                pushing.push(None, report("shizuku.udp_send", 42)),
                Pushed::Coalesced
            ));
        }
        // One report of that kind exists at any moment, whatever the traffic: the rest are a count.
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
        // The summary is the task's to emit: nothing else in the process wakes for a window closing. This is
        // also the clock model - the coalescer's deadlines and the task's sleep are the same clock, so a
        // paused test moves both and this costs no real time at all.
        tokio::time::sleep(WINDOW * 2).await;
        {
            let emitted = emitted.lock().expect("the sink is poisoned");
            assert_eq!(emitted.len(), 2);
            assert_eq!(emitted[1].call_id, Some(8));
            assert_eq!(suppressed(&emitted[1]), Some("1"));
        }
        reporter.finish().await.expect("the flush must complete");
        // Joined, so a window opened after this cannot be closed behind the session's back - and refused, so
        // there is no window to close either.
        assert!(matches!(
            pushing.push(None, report("shizuku.echo_send", 11)),
            Pushed::Closed(_)
        ));
        tokio::time::sleep(WINDOW * 2).await;
        assert_eq!(count(&emitted), 2);
    }

    /// The report a session is most likely to lose: one still inside a window when everything stops. It has
    /// to reach the app exactly once - the flush and the task both emit, and either emitting twice or
    /// neither emitting would be a lie about what happened.
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
        // Due, but the task has not been given a turn to notice, which is where a flush that ran after the
        // join would race it.
        tokio::time::advance(WINDOW * 2).await;
        reporter.finish().await.expect("the flush must complete");
        let emitted = emitted.lock().expect("the sink is poisoned");
        assert_eq!(emitted.len(), 2);
        assert_eq!(suppressed(&emitted[1]), Some("1"));
    }

    /// A conversation that finished has stopped reporting, and a producer that still holds the reporter it
    /// found beforehand must not be able to undo that.
    #[tokio::test(start_paused = true)]
    async fn nothing_is_reported_after_the_conversation_finished() {
        let registry = ReporterRegistry::new();
        let (emitted, sink) = collecting();
        let reporter = registry
            .install(Reporter::new(WINDOW, HANDOFF, sink))
            .expect("the first installation must be accepted");
        let racing = registry.get().expect("a producer finds the reporter");
        reporter.finish().await.expect("the flush must complete");
        // Admission is invalidated globally, so a producer arriving now finds nothing at all.
        assert!(registry.get().is_none());
        // And one that already had it is refused rather than served, so the report is not coalesced into a
        // window either - there is nothing left to close it.
        assert!(matches!(
            racing.push(Some(3), report("shizuku.udp_send", 42)),
            Pushed::Closed(_)
        ));
        tokio::time::sleep(WINDOW * 3).await;
        assert_eq!(count(&emitted), 0);
        // A stale handle carries no way to finish anything either: only a guard can, and there is none.
        drop(racing);
        assert_eq!(count(&emitted), 0);
    }

    /// Two conversations must not overlap, and the refusal must cost nothing: the reporter that was not
    /// installed has to be gone, task and sink with it.
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
        // The only other share of the refused sink went with the reporter that was dropped, so no task is
        // holding one: a spawn before the registration would show up here as a share that outlived it.
        assert_eq!(Arc::strong_count(&refused_emitted), 1);
        assert_eq!(count(&refused_emitted), 0);
        // The installed one still owns reporting, and still works.
        registry
            .get()
            .expect("the first reporter is still the process's")
            .push(None, report("shizuku.tcp_sweep", 7));
        assert_eq!(count(&emitted), 1);
        reporter.finish().await.expect("the flush must complete");
        // Finished, so the next conversation may install its own.
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

    /// A conversation that never got as far as finishing - a setup step that failed after the installation -
    /// still ends the reporter and still gives the registration back, and does both through the same
    /// finalizer rather than through a `Drop` pretending to have done the waiting.
    #[tokio::test(start_paused = true)]
    async fn a_guard_dropped_without_finishing_ends_the_reporter() {
        let registry = ReporterRegistry::new();
        let (abandoned_emitted, sink) = collecting();
        let abandoned = {
            let _guard = registry
                .install(Reporter::new(WINDOW, HANDOFF, sink))
                .expect("the first installation must be accepted");
            let handle = registry.get().expect("a producer finds the reporter");
            // Twice, so one is emitted at once and one is left in a window - which is the thing a `Drop`
            // that cleaned nothing up would leave in a window nobody closes.
            for _ in 0..2 {
                assert!(matches!(
                    handle.push(None, report("shizuku.tun_output", 5)),
                    Pushed::Coalesced
                ));
            }
            handle
        };
        assert!(registry.get().is_none());
        // The finalizer is a task, so this is where it runs. Until it has, the registration is still busy -
        // which is the whole point of not claiming a `Drop` cleaned anything up.
        tokio::time::sleep(WINDOW * 3).await;
        // Admission is closed as well as unreachable, so nothing accumulates in a window nobody will flush.
        assert!(matches!(
            abandoned.push(None, report("shizuku.tun_output", 5)),
            Pushed::Closed(_)
        ));
        // The window it did hold was flushed by the finalizer rather than summarised later by a task nobody
        // cancelled: one immediate report and one flush, and nothing after.
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

    /// A guard dropped where no runtime can own its finalizer.
    ///
    /// There is nothing here that can wait, so nothing here may claim to have waited. Admission closes and
    /// the window task is cancelled, so nothing accumulates and nothing wakes for a window nobody will close;
    /// the registration is *not* released, so this process never admits another conversation's reporter. The
    /// alternative - releasing it because a `Drop` ran - would admit a successor beside a predecessor whose
    /// producers and task nobody joined.
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
        // Outside the runtime from here.
        drop(reporter);
        assert!(registry.get().is_none());
        assert!(matches!(
            handle.push(None, report("shizuku.tun_output", 5)),
            Pushed::Closed(_)
        ));
        // Nothing was flushed, because nothing ran - and the registry says so by refusing every successor.
        assert_eq!(count(&emitted), 1);
        let (_, successor_sink) = collecting();
        assert!(registry
            .install(Reporter::new(WINDOW, HANDOFF, successor_sink))
            .is_err());
    }

    /// One producer, held inside the sink, which is the interleaving the finalizer has to survive: it is
    /// past the closed flag and on its way to a sink whose conversation is being torn down underneath it.
    struct Parked {
        emitted: Arc<Mutex<Vec<NonfatalReport>>>,
        /// Signalled by the sink when a producer has arrived and is being held.
        entered: mpsc::Receiver<()>,
        /// Lets that producer out again.
        release: mpsc::Sender<()>,
        /// Flipped once a finish has returned. Every sink call after the park records what it saw, so one
        /// call that saw this set is proof the finish returned while work was still coming.
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
        // Exactly one call is held: the point is to have one emitter in flight, not to stall the reporter.
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

    /// The race the fence exists for, orchestrated rather than hoped for: a producer is *inside* the sink
    /// when the conversation finishes.
    ///
    /// A finalization that read its counts and joined its task in that gap would return while a report was
    /// still on its way to a writer being torn down. So it has to wait, and the proof that it does is that
    /// `finish` makes no progress at all while the producer is held: under a paused clock a runtime with
    /// nothing runnable advances to the next timer, so the timeout below can only elapse if it is parked.
    #[tokio::test(start_paused = true)]
    async fn nothing_reaches_the_sink_after_finish_returns() {
        let registry = ReporterRegistry::new();
        let (parked, sink) = parking();
        let reporter = registry
            .install(Reporter::new(WINDOW, HANDOFF, sink))
            .expect("the first installation must be accepted");
        let pushing = registry.get().expect("a producer finds the reporter");
        // A real thread, because a producer does not await anything: this is the packet path's own call.
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
        // Nothing has reached the sink twice and nothing has been lost: the one report is in the sink's hands.
        assert_eq!(parked.late.load(Ordering::SeqCst), 0);

        parked.release.send(()).expect("the producer is waiting");
        finishing.await.expect("the flush must complete");
        parked.finished.store(true, Ordering::SeqCst);
        let pushing = producer.join().expect("the producer completes");
        // And a producer that still holds the reporter is refused rather than served, so the promise holds
        // for later reports too.
        assert!(matches!(
            pushing.push(None, report("shizuku.udp_send", 42)),
            Pushed::Closed(_)
        ));
        assert_eq!(count(&parked.emitted), 1);
        assert_eq!(parked.late.load(Ordering::SeqCst), 0);
    }

    /// The other half of that ownership: while a conversation is still finishing, nobody else may install a
    /// reporter. Its producers are still being drained and its task is still being joined, so a successor
    /// here would be a second conversation owning reporting while the first one is still ending.
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
        // Ordinary lookup is already gone, which is what stops a *new* report from finding a reporter that
        // is tearing down.
        assert!(registry.get().is_none());
        // The registration is nevertheless still busy, so this refusal is the successor being ordered behind
        // the predecessor rather than being told the predecessor is still live.
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
        // Complete, so now it is the successor's.
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

    /// A `finish` future dropped part way through gives up the answer and nothing else.
    ///
    /// The three places it can be dropped are the three things the finalizer waits for: the producer drain,
    /// the window join and the final flush. Whichever it was, the finalizer is a task and keeps going - so
    /// the successor stays refused until the work is actually done, and then is admitted.
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
            // Dropped while the finalizer is parked on the producer drain.
            let mut finishing = Box::pin(reporter.finish());
            tokio::time::timeout(WINDOW * 4, &mut finishing)
                .await
                .expect_err("the drain has not completed");
        }
        // Still refused, because dropping the waiter did not finish anything.
        let (_, early_sink) = collecting();
        assert!(registry
            .install(Reporter::new(WINDOW, HANDOFF, early_sink))
            .is_err());

        parked.release.send(()).expect("the producer is waiting");
        producer.join().expect("the producer completes");
        // The finalizer runs to completion on its own, and only true completion admits a successor.
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
        // Exactly one emission from the predecessor: one runner, one flush.
        assert_eq!(count(&parked.emitted), 1);
        successor.finish().await.expect("the flush must complete");
    }

    /// The other boundary a dropped waiter can land on: the finalizer is parked on the one place in the
    /// writer's queue, with a summary still to hand over.
    ///
    /// The window join between the two is the same shape - one `await` inside the same finalizer - and is not
    /// exercised separately here, because parking the window task means blocking inside its sink, which on
    /// the current-thread runtime a paused clock requires would block the test as well.
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
        // The one place is taken by the immediate emission the writer is still holding.
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

        // The writer catches up. The finalizer, which nothing is awaiting any more, hands the summary over.
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

    /// A controller that has stopped reading, for as long as it likes.
    ///
    /// The queue in front of it is unbounded, so this is the case where reporting would grow it without
    /// bound: one summary per window per site, window after window. What has to happen instead is that the
    /// reports stay where the coalescer already bounds them - one batch per site - and are delivered when
    /// room comes back, with nothing lost and nothing counted twice.
    #[tokio::test(start_paused = true)]
    async fn a_blocked_handoff_holds_reports_in_their_windows_across_windows() {
        let registry = ReporterRegistry::new();
        // Every report the sink is given is kept, permit and all, exactly as a frame waiting to be written.
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
        // Two distinct source sites, which is what a batch is keyed by.
        let sites = [42u32, 77u32];
        // The two places in the queue go to the first report of each site, and then there are none left.
        for line in sites {
            assert!(matches!(
                pushing.push(None, report("shizuku.udp_send", line)),
                Pushed::Coalesced
            ));
        }
        assert_eq!(held.lock().expect("the sink is poisoned").len(), 2);
        // Sustained reports across window after window, with the writer taking nothing.
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
        // Nothing more was handed over, because there was nowhere to hand it: the queue is exactly as long as
        // this reporter's share of it, whatever the traffic was.
        assert_eq!(held.lock().expect("the sink is poisoned").len(), 2);

        // The controller reads at last. Finishing is what proves nothing was lost: every report of each site
        // is accounted for by the summary that window kept.
        held.lock().expect("the sink is poisoned").clear();
        reporter.finish().await.expect("the flush must complete");
        let held = held.lock().expect("the sink is poisoned");
        assert_eq!(held.len(), 2);
        for (report, _) in held.iter() {
            assert!(sites.contains(&report.report.line));
            assert_eq!(suppressed(report), Some("50"));
        }
    }

    /// The one permit, and what it is for: the final flush hands one summary over at a time and waits for the
    /// writer to have taken the last one before offering the next.
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
        // A is handed over at once and parks the only permit; B has nowhere to go and waits in its window.
        // Both sites then take a second report, so both have a summary to flush.
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
        // Nothing was handed over while the writer held the permit.
        assert_eq!(taken(&held), 1);

        // The writer writes A and drops the message, which is what releases the place. The first summary goes
        // over and the flush waits again - because there is one place and the writer is now holding that.
        held.lock().expect("the sink is poisoned").remove(0);
        tokio::time::timeout(WINDOW * 4, &mut finishing)
            .await
            .expect_err("the flush must wait between summaries");
        assert_eq!(taken(&held), 1);

        // The second summary follows the second release, and only then - and that is the last of them.
        held.lock().expect("the sink is poisoned").remove(0);
        finishing.await.expect("the flush must complete");
        assert_eq!(taken(&held), 1);
        held.lock().expect("the sink is poisoned").remove(0);
        assert_eq!(taken(&held), 0);
    }

    /// The same wait, with a writer that goes away instead of catching up. Its held message is dropped, which
    /// releases the place, and the next send returns the real failure rather than parking on a peer that is
    /// gone.
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
        // The writer goes away: what it was holding is dropped, place and all.
        connected.store(false, Ordering::SeqCst);
        held.lock().expect("the sink is poisoned").clear();
        let failed = finishing
            .await
            .expect_err("a report the writer could not take has to reach the session's result");
        assert!(failed.to_string().contains("could not be delivered"));
    }
}
