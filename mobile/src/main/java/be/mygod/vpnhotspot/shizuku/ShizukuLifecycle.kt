package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.job
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * One rootless session's lifespan: a single cancellable [Job], its predecessor barrier, and the finalizer
 * that is the only teardown. The shape is `RepeaterService`'s, because the problem is the same one.
 *
 * This owns *ordering* and nothing else. It creates no scope and no liveness of its own and publishes no
 * state: the jobs it installs are launched on the owner scope passed to it - `ShizukuTetheringService`'s own
 * main-confined scope in production - and every observable fact, the user's intent, a committed session's
 * label, whether the process may let go, is [Session]'s to publish. Which is what makes it the real owner
 * rather than a mirror of one: there is exactly one job, held here, and everything else reads through it.
 *
 * A job here means exactly one thing: a lifespan that is in flight, and [lifespan] holds at most one of them
 * - the most recent. A start installs its job before starting it, and from then on the lifespan is either
 * still the current one or has been *superseded*: a successor may replace the field at any point, including
 * while this one is part-way through its own finalizer. Nothing is ever kept behind after completion to
 * stand for something else.
 *
 * The clearing is guarded by identity, and the invariant is exactly that split. A lifespan still current
 * when its final bookkeeping lands stays installed until that moment, then clears itself and settles once. A
 * superseded one is no longer installed - its successor is - so it fails the guard deliberately: it clears
 * nothing and settles nothing, because neither the field nor the settlement is its to speak for any more.
 * Concrete resource debt is the ledger's to remember, and [Session.settled] is how the owner hears about
 * it.
 *
 * The *barrier* alone reaches further than one instance, in [installed], and it has to: the generations it
 * orders belong to the process, while a lifecycle belongs to a component Android may destroy in the middle
 * of one. So every command that installs a lifespan - a start, an idle [housekeep], a [destroy] - joins the
 * last lifespan installed anywhere, and only that, and each becomes that barrier in turn for whatever comes
 * next. Liveness stays where it was - no instance cancels another's job, answers [idle] for it or settles on
 * its behalf - which is the
 * one distinction the two references exist to keep, and why the destructive half of a teardown is keyed on
 * the lifespan that owns it rather than on whatever the ledger currently holds.
 *
 * Both commands return immediately. A start installs a lifespan; a stop cancels it. Cancellation at any
 * suspension point unwinds into the same finalizer, which is why there is no starting or stopping state to
 * render and nothing for a caller to wait behind.
 *
 * [housekeep] and [destroy] are the commands the user never presses, and both install the same cleanup-only
 * lifespan: one that publishes nothing, acquires nothing and is the finalizer alone. [housekeep] is for the
 * commands that install and cancel nothing - a duplicate press, an idle stop - because *idle* says only that
 * no lifespan of this instance is in flight, never that the process owes nothing: what a previous teardown
 * could not fence is still there, and without a start there is nothing to retry it or to wind the component
 * down afterwards. [destroy] installs that same lifespan *over* whatever was running, because destruction is
 * the last command this owner will ever get and a retirement that fails under it has nothing behind it at
 * all. Both take the same barrier, make the same single cleanup attempt and settle through the same
 * bookkeeping, deliberately: an owner that ran beside those would be a second lifecycle rather than part of
 * this one.
 *
 * Everything here is local to this mode. It does not consult, start, stop, delay or refuse root mode, and
 * root mode does not consult it.
 *
 * @param scope the owner's own scope, which must be single-threaded and must outlive a teardown.
 */
class ShizukuLifecycle(private val scope: CoroutineScope, private val session: Session) {
    /** The one session a lifespan drives, and every effect it has on the world outside this class. */
    interface Session {
        /**
         * Settles whatever debt a previous generation left behind, and creates nothing.
         *
         * Split from [prepare] and ordered in front of it because the two answer to different things. This
         * one is the *old* generation's, and what it takes to fence a child, close a descriptor or withdraw
         * an agent is local: no fresh authorization, no device support and no live tethering service.
         * Everything [prepare] asks is the *new* generation's, and any of those may refuse one permanently.
         * Behind them, a Shizuku that has gone away or a tethering service that died would take the only
         * retry a recoverable child had with it - so a failed successor gate refuses the new session and
         * leaves the old one's cleanup already done.
         *
         * Called by a live lifespan as that start's one attempt at the debt, and by a cleanup-only one as
         * the whole of what it is for. Either way only after the process predecessor has completed, which is
         * what makes it safe for it to name no generation: the one it finds is neither still being withdrawn
         * nor yet to be published, because a successor from any component orders itself behind this
         * lifespan. Throwing leaves the debt as it was and stops a live lifespan before it acquires
         * anything.
         */
        suspend fun settle()

        /**
         * Everything that has to hold before this generation creates anything *of its own*: authorization,
         * device support, and the checks only a new session has to pass. Returns the publication step.
         *
         * Reached with the previous generation already settled, so it never decides what to do about one,
         * and refusing this session cannot reach backwards into the last one's cleanup.
         *
         * Cancellable throughout, which is the whole reason no starting state is needed - a stop arriving
         * during the user's permission dialog unwinds this rather than queueing behind it.
         */
        suspend fun prepare(): suspend () -> Unit

        /**
         * Suspends for as long as the published session runs, returning once it has lost the machinery it
         * needs. Returning is an autonomous ending; the lifespan treats it exactly as it treats a stop.
         */
        suspend fun awaitEnd()

        /**
         * Withdraws this generation completely - its child, its descriptor, its agent, and everything a
         * failed publication managed to create.
         *
         * Called from a finalizer, and only by a lifespan that may have acquired something of its own.
         * [owner] is that lifespan, and it is the whole of this call's authority to destroy: a generation
         * belongs to the lifespan that published it, so one the implementation finds under a different owner
         * is not this call's to take back, however far along its own finalizer this one is. Passed rather
         * than read back out of published intent, which is already off by the time this runs and never named
         * a generation in the first place.
         *
         * Throwing is reported and nothing more: it says a withdrawal did not finish, not *where* it
         * stopped. It may have left local resources behind, or it may have fenced every one of them and
         * failed only on the privileged release that follows them. This class never infers one from the
         * other - it reports the failure, then asks [fenced] and settles from that answer, which is the
         * ledger's rather than this throw's.
         */
        suspend fun retire(owner: Job)

        /**
         * Accepts [owner] as the lifespan the user asked for. Immediate, and not a transition: the row
         * reads on from here, whatever this lifespan still has to do before it commits anything.
         */
        fun publish(owner: Job)

        /**
         * Withdraws [owner] as the accepted lifespan, if it still is one.
         *
         * Identity-guarded by the implementation, so a lifespan finalizing after a successor was accepted
         * cannot turn the row off under it. Published *before* a teardown begins, not after it finishes.
         */
        fun withdraw(owner: Job)

        /** Makes one operational failure visible. */
        fun report(e: Exception)

        /**
         * Whether the ledger says every local resource this process created is proven gone - nothing
         * outstanding, which is wider than nothing hopeless: a resource whose release was issued but never
         * confirmed may still be relaying.
         *
         * False is the one case where the owner must keep holding the process rather than winding down as
         * though the teardown had succeeded. Suspending because the ledger is confined to a lane this has to
         * reach, and asked of it rather than remembered here precisely because a component's own memory of a
         * debt does not survive that component being recreated, while the child it described does. Throwing
         * counts as false: a ledger that could not be reached is not one that said nothing is owed.
         */
        suspend fun fenced(): Boolean

        /**
         * This lifespan is over and its job has already been cleared. Reached exactly once by the lifespan
         * that was still the installed one when its final bookkeeping ran, even when the retirement, the
         * fence query or their reporting threw - and not at all by one a successor has already replaced,
         * which may not settle on that successor's behalf.
         *
         * [fenced] is the ledger's answer, not this class's: false means concrete local resource debt
         * remains, which is the owner's cue to keep holding the process even though nothing is in flight
         * any more. Nothing here remembers that debt: what retries it is [settle], and every later command
         * reaches it - a start in front of its own preparation, an idle stop or a destruction as the whole
         * of the cleanup-only lifespan it installs.
         */
        fun settled(fenced: Boolean)
    }

    companion object {
        /**
         * The most recently installed lifespan in this process, or null when the one that installed it has
         * completed without having been replaced. An ordering reference and nothing else, and exactly two
         * things touch it: [install] reads it to capture the predecessor the lifespan it is building will
         * join, and a lifespan reaching its final bookkeeping clears it if it still finds itself here.
         * Nothing is ever cancelled through it, nothing is owned through it, and it never answers [idle] -
         * the job it names may belong to an instance whose component Android has already removed.
         *
         * It exists because [lifespan] cannot reach far enough. A destroyed service does not necessarily
         * leave nothing in flight - `onDestroy` deliberately keeps its own scope alive until the installed
         * lifespan's finalizer has settled - so an instance recreated behind one starts with an empty
         * [lifespan] and would otherwise authorize, retry that finalizer's debt and acquire straight through
         * it, or read the ledger through it and strand itself on what it saw. The generations are the
         * process's, so the ordering that keeps them one at a time has to be the process's too, and this is
         * the smallest thing that can be: one reference, joined and never owned.
         *
         * Confined to the owner scope, `Dispatchers.Main.immediate` in production, and guarded by identity
         * exactly as [lifespan] is - a lifespan clears this only if it still finds itself here. Nothing is
         * persisted and nothing outlives the process, which is the last fence under this one too.
         */
        private var installed: Job? = null
    }

    /**
     * The most recent lifespan in flight *of this instance*, or null when none is, and this instance's own
     * command and liveness authority: what a stop cancels, what [idle] answers from, and whose settlement
     * winds this owner down. Distinct from [installed] for exactly one reason - a successor from another
     * instance may order itself behind this job without inheriting the right to speak for this owner.
     *
     * Every command that installs a lifespan writes it before starting that job, and it holds it -
     * cancelled or not - until either its own finalizer clears it or a successor of this instance's
     * replaces it, whichever comes first.
     *
     * Confined to [scope], and its *identity* is the whole guard - the same one `RepeaterService` uses. A
     * lifespan reaching its final bookkeeping clears this only if it still finds itself here; one a
     * successor has already replaced finds the successor instead and leaves it alone, which is exactly why
     * the clearing cannot be unconditional. Either way nothing outlives a job's completion: what a completed
     * lifespan may have left behind is the ledger's to answer for, not this field's.
     */
    private var lifespan: Job? = null

    /**
     * What a lifespan's finalizer still has to settle, which is also the whole of its authority to destroy
     * anything. A local of the job rather than a field, because it is a fact about one lifespan's progress
     * and there is nothing for anyone else to read it for.
     *
     * It moves one way only, as the lifespan reaches further, and every ending is answered from wherever it
     * had got to.
     */
    private enum class Owed {
        /**
         * Nothing this finalizer may take back. A live lifespan starts here because the settlement in its
         * own body is the one attempt its start gets at whatever it inherited - so one cancelled on the
         * barrier leaves that debt to the predecessor whose finalizer already owns it, and one whose
         * settlement or preparation failed does not immediately try the same thing again.
         */
        NOTHING,

        /**
         * A previous generation's debt, which this lifespan exists to attempt and has not attempted yet.
         * The one case in which a finalizer settles something it did not publish, and the reason it is
         * sound is the barrier: it settles after the whole process predecessor, and every successor orders
         * itself behind it.
         */
        INHERITED,

        /**
         * The generation this lifespan itself published, wholly or in part. Set before the publication step
         * rather than after it, because a publication that fails part-way through has still created things
         * this finalizer owes back.
         */
        OWN,
    }

    /**
     * Installs one lifespan on the owner's scope and starts it, which is the whole of every command here.
     *
     * [live] is the only difference between them, and it is one of *purpose* rather than of machinery. A
     * live lifespan is the one the user asked for: it is published as the accepted owner and it runs the
     * cancellable half - the barrier, the inherited settlement, the preparation, the publication and the
     * session itself - before the finalizer that takes back whatever it reached. A cleanup-only one is the
     * component's own, is published to nobody, and is that finalizer alone: everything it does is teardown,
     * so there is no suspension in it for a cancellation to land on and nothing a stop could take back.
     *
     * Both go through the same barrier, the same single cleanup attempt and the same final bookkeeping, so
     * the ordering, the identity guards and the settlement are one implementation and not two.
     */
    private fun install(live: Boolean) {
        // Retained as the barrier rather than joined here: a successor is accepted and shown at once, but
        // may not authorize, retry a predecessor's debt or acquire anything until that predecessor's
        // teardown has finished, because one generation at a time is the whole of the ledger's rule.
        //
        // Taken from the process rather than from this instance, because a predecessor need not be one of
        // this instance's: a service Android destroyed mid-teardown leaves its lifespan finalizing on a
        // scope kept alive for it, and the instance recreated behind that is this one.
        val predecessor = installed
        val job = scope.launch(start = CoroutineStart.LAZY) {
            val self = coroutineContext.job
            var owed = if (live) Owed.NOTHING else Owed.INHERITED
            try {
                // A cleanup-only lifespan has no cancellable half at all - it is the finalizer below and
                // nothing else - so it joins its predecessor there, in the one place both kinds do.
                if (live) try {
                    predecessor?.join()
                    // In front of every successor-only prerequisite, because none of them is what fences a
                    // child, closes a descriptor or withdraws an agent. Authorization, device support and
                    // the rest belong to the session this start wants; behind them, a Shizuku that has gone
                    // away or a tethering service that died would take the last generation's only remaining
                    // retry with it. Failing here stops this start before it acquires anything, which is
                    // the point: the ledger admits one generation, and this is the one that clears it.
                    session.settle()
                    val publish = session.prepare()
                    // From here this lifespan may own part of a generation, so its finalizer is the one
                    // thing that withdraws it. Set before the publication rather than after, because a
                    // publication that throws part-way through has already created things.
                    owed = Owed.OWN
                    publish()
                    session.awaitEnd()
                } catch (e: CancellationException) {
                    // An ordinary cancellation is a stop, possibly still behind the barrier, and says
                    // nothing worth reporting. A deadline is an operational failure however it is shaped,
                    // and `withTimeout` shapes it as one of these.
                    if (e is TimeoutCancellationException) session.report(e)
                } catch (e: Exception) {
                    session.report(e)
                }
                // Reached by every ending, including a fatal one on its way out: a `finally` rather than
                // ordinary code after the catches, so a failure while *reporting* cannot skip the teardown.
            } finally {
                withContext(NonCancellable) {
                    try {
                        // Off before the teardown rather than after it: what the user asked for is already
                        // settled, and the work that remains is this mode's. Identity-guarded inside the
                        // session, so a successor accepted meanwhile keeps the row on. A cleanup-only
                        // lifespan published nothing, so the same guard makes this its no-op.
                        session.withdraw(self)
                        // Re-joined because the barrier above may have been abandoned mid-wait, and a
                        // successor joining *this* lifespan has to inherit that ordering. For a
                        // cleanup-only lifespan it is the first join rather than the second, and it is what
                        // puts the settlement below behind a teardown that is still finishing rather than
                        // beside it.
                        predecessor?.join()
                        try {
                            when (owed) {
                                // Either nothing was ever created, or this start has already made its one
                                // attempt at what it found. A second here would be the same debt attempted
                                // twice in a row and reported twice, by one press.
                                Owed.NOTHING -> {}
                                Owed.INHERITED -> session.settle()
                                Owed.OWN -> session.retire(self)
                            }
                        } catch (e: CancellationException) {
                            // Already inside [NonCancellable], so this is a deadline or a lost epoch rather
                            // than a caller going away: the withdrawal did not finish, whatever shape the
                            // cancellation arrived in, and a silent one would be a teardown nobody hears of.
                            session.report(e)
                        } catch (e: Exception) {
                            session.report(e)
                        }
                        // Nested so that a retirement that threw past its own handling, or a *reporter* that
                        // threw, still cannot skip the bookkeeping below - and so a fatal throwable
                        // propagates afterwards rather than being swallowed by it.
                    } finally {
                        // Holds the conservative answer from the start, so "could not ask" and "answered no"
                        // are the same outcome: something may still be relaying. Only a query that returned
                        // overwrites it.
                        var fenced = false
                        try {
                            try {
                                // Asked of the ledger rather than remembered here: whether this process
                                // still owes a child, a descriptor or an agent is a fact about the
                                // generation, and it outlives every component that might keep a flag on it.
                                fenced = session.fenced()
                            } catch (e: CancellationException) {
                                session.report(e)
                            } catch (e: Exception) {
                                session.report(e)
                            }
                            // Nested once more so a *reporter* that threw cannot skip the two lines below
                            // either. They are this lifespan's last act and they are not conditional on
                            // anything having gone right - only on this still being the installed job, so a
                            // finished one cannot clear or settle a successor's.
                        } finally {
                            // Two references and two guards, because they answer different questions. The
                            // process-wide one is the next start's ordering fence, and a successor from any
                            // instance may already have taken it over; the local one is this instance's own
                            // liveness, which no other instance ever writes. So a lifespan can quite
                            // ordinarily have stopped being anyone's predecessor while still being the one
                            // thing its own owner is waiting on - and it clears each exactly where it is
                            // still the one installed.
                            if (installed === self) installed = null
                            if (lifespan === self) {
                                lifespan = null
                                session.settled(fenced)
                            }
                        }
                    }
                }
            }
        }
        // Both, and before the job is started: the process-wide one is what the *next* start anywhere will
        // order itself behind, and the local one is what this instance's own stop cancels. A cleanup-only
        // lifespan takes both for the same two reasons - it settles a generation, so nothing may overtake
        // it, and it is this instance's one liveness authority while it runs.
        installed = job
        lifespan = job
        // Accepted before the lazy job starts, so the row reads on from the moment the press is taken and
        // anything this lifespan commits already has an owner to be stamped against. A cleanup-only lifespan
        // is nobody's intent: it publishes nothing, and the row stays exactly as the stop before it left it.
        if (live) session.publish(job)
        job.start()
    }

    /**
     * Accepts a start and returns. A press while intent is already on is that same intent and installs
     * nothing; the caller owns that check, because it owns the published intent.
     */
    fun start() = install(true)

    /**
     * Takes over a command that installed and cancelled nothing - a duplicate press, an idle stop - and
     * returns.
     *
     * Requires [idle], and the caller owns that check for the same reason it owns the intent one: this
     * cancels nothing, so installing it over a live lifespan would leave that lifespan running with no owner
     * able to settle it. A destruction needs the cancellation too and is [destroy] for exactly that reason.
     *
     * What it installs is a lifespan that publishes nothing, prepares nothing and acquires nothing: its
     * whole body is the finalizer, so it joins the process predecessor, makes one attempt at whatever that
     * predecessor could not settle, and then asks the ledger and settles this owner from the answer.
     *
     * The two things it fixes are the two an idle command otherwise has no answer for. Recoverable debt left
     * by a teardown that could not fence its resources has no retry unless a start happens to come; and a
     * component recreated behind a finalizer that is still running would otherwise read the ledger straight
     * through that finalizer, see resources it is in the middle of releasing, and keep itself foreground
     * forever waiting for an owner that already settled somebody else's scope.
     *
     * Both are the same missing thing - an owner - so this installs one rather than adding a query beside
     * the lifespan. It is superseded by an ordinary start exactly as any lifespan is, and settles nothing
     * once it has been.
     */
    fun housekeep() = install(false)

    /**
     * Asks for a stop and returns. Intent goes off at once, whatever the teardown still has to do; with no
     * lifespan in flight there is no owner to withdraw and nothing to cancel, and whatever the ledger may
     * still hold is [housekeep]'s or [destroy]'s to attempt.
     *
     * A cleanup-only lifespan is cancelled here like any other and is unmoved by it, which is the point of
     * its having no cancellable half: it published no intent to withdraw and holds no suspension for the
     * cancellation to land on, so a second idle stop rides on the attempt already in flight rather than
     * starting a second one beside it.
     */
    fun stop() = stop(lifespan)

    /**
     * Withdraws [job] as the accepted owner and then cancels it, in that order: what the user asked for is
     * answered at once, and the teardown that follows is this mode's own business. Null is the ordinary
     * case of nothing in flight and does nothing at all.
     *
     * Takes the lifespan rather than reading it, because [destroy] has to name one it is no longer the
     * installed lifespan.
     */
    private fun stop(job: Job?) {
        if (job == null) return
        session.withdraw(job)
        job.cancel()
    }

    /**
     * Takes this component's own destruction and returns. Not a choice between [stop] and [housekeep] but
     * exactly both, in the one order that is safe.
     *
     * That order is the whole of the method. A cancellation can resume the outgoing lifespan *inline* -
     * neither `Dispatchers.Main.immediate` nor the test's `Unconfined` queues a resumption onto the thread
     * it is already on - so one cancelled while it is still the installed lifespan can run its entire
     * finalizer before the next statement here: clear both references, settle this owner, and, because the
     * component is gone by then, end the scope. A successor installed after that would go onto a dead scope,
     * behind a predecessor that had already finished, over debt nothing would ever attempt again. Installed
     * *first*, that same finalizer fails both identity guards - it clears nothing and settles nothing - and
     * the successor already parked on its join is what settles, after its own attempt.
     *
     * Which is also why destruction does not branch on an instantaneous [idle]. It is the last command this
     * owner will get: a retirement that fails under it has nothing behind it, and a component destroyed over
     * recoverable debt abandons a child that may still be relaying in a process an activity or another
     * foreground service keeps alive. So it always leaves one owner and always makes one attempt, whether or
     * not anything was running when it arrived - and when nothing was, the cancellation below is the no-op
     * an idle stop's already is.
     */
    fun destroy() {
        val outgoing = lifespan
        install(false)
        stop(outgoing)
    }

    /**
     * No lifespan in flight, so no job will call [Session.settled] and nothing will wind an owner down on
     * its behalf. It says nothing about resource debt: that question is the ledger's, and an owner that
     * finds this true has neither an answer to it nor anything left that could act on one - which is what
     * [housekeep] installs, rather than the owner asking the ledger itself.
     */
    val idle get() = lifespan == null
}
