package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineExceptionHandler
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.job
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotSame
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.Timeout
import java.io.IOException

/**
 * What one lifespan promises: both commands return at once, exactly one teardown runs however the lifespan
 * ended, a successor may be accepted immediately but never overtakes its predecessor's cleanup, and the
 * lifespan still installed when its final bookkeeping lands clears itself and settles once whatever failed
 * on the way out - while a superseded predecessor clears and settles nothing, which is asserted here too.
 *
 * Every wait below is on the condition itself rather than on a yield count or a clock: the recorder
 * publishes its own progress, and a test that asks for a state the owner will not reach blocks rather than
 * passing by accident. The whole thing runs on `runBlocking`'s single event loop, which is the confinement
 * the production owner has too - its scope is the service's main thread.
 *
 * The recorder is a fake, and these tests claim only what a fake can prove: the *ordering* contract
 * [ShizukuLifecycle] implements and the projection rule [OwnedState] implements. Nothing here exercises
 * `ShizukuTestNetwork`'s own phases, resources or dispatcher - those need the framework and are covered by
 * the source contract in `docs/vpnhotspotd/shizuku.md` instead.
 */
class ShizukuLifecycleTest {
    /**
     * A failure bound and never a synchronization device: every wait below is on a condition the owner
     * either reaches or does not, and this only decides how long a regression is allowed to hang CI before
     * it is called a failure.
     */
    @get:Rule
    val bound: Timeout = Timeout.seconds(20)

    /** The one side, recorded in order so a test can assert *when* something ran, not just whether. */
    private class Recorder(
        private val publishFails: Throwable? = null,
        var withdrawal: Withdrawal = Withdrawal.Fences,
        var settleFails: Throwable? = null,
        var prepareFails: Throwable? = null,
        private val reportFails: Throwable? = null,
    ) : ShizukuLifecycle.Session {
        /**
         * What a withdrawal does when it runs, and the only thing that moves [debt].
         *
         * The cause travels with the outcome rather than beside it, because in production the two are one
         * decision: a withdrawal either proved the local resources gone or did not, and *where* it failed is
         * what decides which. Fencing the child, withdrawing the agent and closing the descriptor happen
         * before the privileged release, so a failure after all three leaves nothing local owed even though
         * the caller still hears about it.
         */
        sealed interface Withdrawal {
            /** Everything local is proven gone and nothing is thrown. */
            data object Fences : Withdrawal
            /** It threw before it could prove the local resources gone, so the debt stands. */
            class LeavesLocalDebt(val cause: Throwable) : Withdrawal
            /**
             * Local fencing finished and only the privileged release could not be confirmed - the residual
             * case, which the caller is told about but which owes nothing local.
             */
            class LeavesResidual(val cause: Throwable) : Withdrawal
        }
        /**
         * The one fact the ledger stands for here, and the only state `settle`, `retire` and `fenced` derive
         * from: this process still owes *local* resources it created - the exact question
         * [ShizukuLifecycle.Session.fenced] asks. Privileged residue is a different debt, retried inside
         * `ShizukuTestNetwork.settle`, and nothing here models it.
         *
         * One variable rather than several coordinated flags, because in production they are one thing.
         * *Publishing* is what first incurs it, exactly as a live generation's TUN, child and agent are all
         * outstanding in the real ledger; a [Withdrawal.LeavesLocalDebt] outcome is what leaves it standing -
         * not merely a withdrawal that threw, since [Withdrawal.LeavesResidual] throws with everything local
         * already proven gone - a settlement that succeeded is what clears it, and the fence query is that
         * same fact read back. So no test can arrange a combination the real ledger could not be in, and a
         * fence query taken while a session is running or being withdrawn answers false, as the real one
         * does.
         */
        var debt = false

        private val entries = MutableStateFlow(emptyList<String>())
        val log get() = entries.value
        val reported = mutableListOf<Exception>()

        /**
         * Deliberate failures the harness caught coming out of a lifespan, so a test can assert on the one
         * it arranged instead of letting it print as an unexplained stack trace beside a passing run.
         */
        private val uncaught = mutableListOf<Throwable>()
        val handler = CoroutineExceptionHandler { _, e -> uncaught += e }
        /**
         * Unwrapped, because kotlinx's stack-trace recovery hands a handler a *copy* of the exception whose
         * cause is the original: identity has to be asserted through that rather than against the copy.
         */
        fun takeUncaught() = uncaught.map { it.cause ?: it }.also { uncaught.clear() }
        fun assertNothingUncaught() = assertEquals("a lifespan failed in a way no test arranged",
            emptyList<Throwable>(), uncaught)

        /** Every intent transition, so a test can assert the order of off against the teardown after it. */
        val intents = mutableListOf<Boolean>()

        /** What the owner last said about itself, or null while it has not settled at all. */
        var fenced: Boolean? = null
            private set
        var settlements = 0
            private set

        /** Held open by a test that needs a step still running when it looks. */
        var settleGate: CompletableDeferred<Unit>? = null
        var prepareGate: CompletableDeferred<Unit>? = null
        var publishGate: CompletableDeferred<Unit>? = null
        var retireGate: CompletableDeferred<Unit>? = null
        var fenceGate: CompletableDeferred<Unit>? = null

        /** Makes the ledger unreachable, which is not the same as it answering that nothing is owed. */
        var fenceFails: Throwable? = null
        var fenceQueries = 0
            private set

        /** Completed by a test to end a committed session the way its own machinery failing would. */
        var terminal = CompletableDeferred<Unit>()

        private fun record(entry: String) {
            entries.value = entries.value + entry
        }

        /**
         * Blocks until the log *starts* with this, which is the only kind of waiting these tests do.
         *
         * A prefix rather than the whole list, because the flow behind it conflates: an append-only log can
         * pass through a value no collector observes, so a predicate that must catch one exactly is a
         * predicate that can miss. A prefix, once true, stays true.
         */
        suspend fun awaitLog(vararg expected: String) {
            val prefix = expected.toList()
            entries.first { it.size >= prefix.size && it.subList(0, prefix.size) == prefix }
        }

        /**
         * The one attempt a lifespan gets at what a previous generation left. Failing leaves the debt
         * exactly as it was; returning is what discharges it.
         *
         * Silent with nothing owed, as production's is: `ShizukuTestNetwork.settle` finds no generation and
         * returns without touching anything, so a log entry here would claim an attempt that never happened.
         */
        override suspend fun settle() {
            if (!debt) return
            record("settle")
            settleGate?.await()
            settleFails?.let { throw it }
            debt = false
        }

        override suspend fun prepare(): suspend () -> Unit {
            record("prepare")
            prepareGate?.await()
            prepareFails?.let { throw it }
            return {
                record("publish")
                // Acquisition is what incurs the debt, and it is incurred as the publication proceeds rather
                // than once it succeeds: a publication that throws part-way has still created things.
                debt = true
                publishGate?.await()
                publishFails?.let { throw it }
            }
        }

        override suspend fun awaitEnd() {
            record("run")
            terminal.await()
            record("ended")
        }

        /**
         * Whose lifespan asked for the most recent withdrawal. In production this is what the ledger checks
         * the running generation's own owner against before it destroys anything, and here it is simply
         * recorded: what a fake can prove is that the finalizer names *itself* rather than whoever the
         * accepted owner happens to be by then.
         */
        var retiredBy: Job? = null
            private set

        override suspend fun retire(owner: Job) {
            retiredBy = owner
            record("retire")
            retireGate?.await()
            when (val outcome = withdrawal) {
                Withdrawal.Fences -> debt = false
                is Withdrawal.LeavesLocalDebt -> {
                    debt = true
                    throw outcome.cause
                }
                is Withdrawal.LeavesResidual -> {
                    debt = false
                    throw outcome.cause
                }
            }
        }

        /** Suspending, as the real one is: the ledger lives on a lane this call has to reach. */
        override suspend fun fenced(): Boolean {
            // Counted rather than logged: the log is the sequence of things done to the session, and asking
            // the ledger a question does nothing to it. Ordering is asserted through [fenceGate] instead.
            fenceQueries++
            fenceGate?.await()
            fenceFails?.let { throw it }
            return !debt
        }

        /**
         * The accepted lifespan, which is what makes both directions below identity-guarded, as in
         * production.
         */
        var owner: Job? = null
            private set

        override fun publish(owner: Job) {
            this.owner = owner
            intents += true
            record("on")
        }

        override fun withdraw(owner: Job) {
            if (this.owner !== owner) return
            this.owner = null
            intents += false
            record("off")
        }

        override fun report(e: Exception) {
            reported += e
            reportFails?.let { throw it }
        }

        override fun settled(fenced: Boolean) {
            settlements++
            this.fenced = fenced
            record(if (fenced) "settled" else "unfenced")
        }
    }

    /**
     * Drives one lifecycle on a scope this test owns, and leaves nothing running: production's scope belongs
     * to a service that outlives its teardown, so a test that simply returned would hang `runBlocking` on a
     * lifespan behaving exactly as designed.
     */
    private fun driving(
        session: Recorder = Recorder(),
        block: suspend CoroutineScope.(ShizukuLifecycle, Recorder) -> Unit,
    ) = runBlocking {
        // Unconfined stands in for the service's `Dispatchers.Main.immediate`, and the difference matters:
        // both run a started job's body inline rather than queueing it, so a lifespan cancelled in the same
        // turn it was installed has still entered its `finally`. A queueing dispatcher would let a start
        // followed immediately by a stop complete a job whose body never ran, which is not a state the
        // production owner can be in - it installs and starts from the main thread it is confined to.
        // A supervisor job and the recorder's own handler so a test that deliberately makes a lifespan fail
        // - a reporter that throws - asserts on that failure instead of cancelling the test runner or
        // printing it beside a passing run. Nothing else about the ordering differs from the service's scope.
        val owner = CoroutineScope(coroutineContext + Dispatchers.Unconfined + SupervisorJob() +
                session.handler)
        val lifecycle = ShizukuLifecycle(owner, session)
        try {
            block(lifecycle, session)
        } finally {
            // Opened before cancelling, because a finalizer is [kotlinx.coroutines.NonCancellable]: one
            // parked on a gate would never observe the cancellation meant to release it.
            session.settleGate?.complete(Unit)
            session.prepareGate?.complete(Unit)
            session.retireGate?.complete(Unit)
            session.fenceGate?.complete(Unit)
            session.terminal.complete(Unit)
            owner.coroutineContext.job.cancelAndJoin()
        }
        session.assertNothingUncaught()
        // The predecessor barrier is the *process's*, so a lifespan left installed by one test would be the
        // one the next test's first start joins - and an *incomplete* one would hang it rather than fail it.
        // That is the leak worth catching, and it is the one this catches: the local field a test can see is
        // cleared in the same final block as the static one, so a scope that has been cancelled and joined
        // leaving this idle has no lifespan still in flight to strand there.
        //
        // It does not observe the static reference itself, which is private and stays that way. That the
        // reference is *cleared* rather than left holding a completed job is a source-audited invariant, and
        // one no test here could fail on in any case: joining a job that has already completed returns at
        // once, so a retained completed one would order nothing and hang nothing.
        assertTrue("a lifespan outlived the scope that owned it", lifecycle.idle)
    }

    /** A start returns while its own preparation is still gated, and intent is on from that moment. */
    @Test
    fun aStartReturnsAndPublishesOnWhileStartupIsGated() = driving { lifecycle, session ->
        session.prepareGate = CompletableDeferred()
        lifecycle.start()
        assertEquals("intent is published before anything suspends", listOf(true), session.intents)
        assertFalse(lifecycle.idle)
        session.awaitLog("on", "prepare")

        session.prepareGate!!.complete(Unit)
        session.prepareGate = null
        session.awaitLog("on", "prepare", "publish", "run")
    }

    /**
     * A stop publishes off at once and cancels a startup that had not finished. That startup acquired
     * nothing, so there is nothing to retire and the lifespan only settles.
     */
    @Test
    fun aStopPublishesOffImmediatelyAndCancelsStartup() = driving { lifecycle, session ->
        session.prepareGate = CompletableDeferred()
        lifecycle.start()
        session.awaitLog("on", "prepare")

        lifecycle.stop()
        assertEquals("off the moment it is asked for", listOf(true, false), session.intents)

        session.prepareGate!!.complete(Unit)
        session.prepareGate = null
        // A cancelled startup never reached its publication, so it acquired nothing and retires nothing.
        session.awaitLog("on", "prepare", "off", "settled")
        assertTrue(lifecycle.idle)
        assertEquals(1, session.settlements)
    }

    /** A lifespan that reached a committed session tears it down exactly once, and only then settles. */
    @Test
    fun aStoppedSessionRetiresExactlyOnce() = driving { lifecycle, session ->
        lifecycle.start()
        session.awaitLog("on", "prepare", "publish", "run")

        lifecycle.stop()
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "settled")
        assertEquals(true, session.fenced)
        assertTrue(lifecycle.idle)
    }

    /**
     * An autonomous ending publishes off *before* its cleanup, not after it.
     *
     * The row is the user's answer to "is this meant to be on", and a session that has already lost its
     * machinery is not. Gated cleanup is what makes the order observable rather than incidental.
     */
    @Test
    fun anAutonomousEndPublishesOffBeforeCleanup() = driving { lifecycle, session ->
        lifecycle.start()
        session.awaitLog("on", "prepare", "publish", "run")

        session.retireGate = CompletableDeferred()
        session.terminal.complete(Unit)
        session.awaitLog("on", "prepare", "publish", "run", "ended", "off", "retire")
        assertEquals("off is published before the teardown, not after it", listOf(true, false),
            session.intents)
        assertNull("and the owner has not settled while cleanup runs", session.fenced)
        assertFalse("the lifespan is still installed for the whole of its own finalizer", lifecycle.idle)

        session.retireGate!!.complete(Unit)
        session.retireGate = null
        session.awaitLog("on", "prepare", "publish", "run", "ended", "off", "retire", "settled")
        assertTrue(lifecycle.idle)
    }

    /**
     * A successor is accepted while a predecessor's cleanup runs, but acquires nothing until it finishes.
     *
     * Mutation-sensitive by construction: drop the barrier join and the successor's `prepare` appears in
     * the log while the predecessor is still retiring.
     */
    @Test
    fun aSuccessorWaitsForItsPredecessorsCleanup() = driving { lifecycle, session ->
        lifecycle.start()
        session.awaitLog("on", "prepare", "publish", "run")
        session.retireGate = CompletableDeferred()
        lifecycle.stop()
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire")

        lifecycle.start()
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "on")

        session.retireGate!!.complete(Unit)
        session.retireGate = null
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "on",
            "prepare", "publish", "run")
    }

    /** A successor cancelled while still waiting behind its predecessor never publishes anything. */
    @Test
    fun aCancelledWaitingSuccessorNeverPublishes() = driving { lifecycle, session ->
        lifecycle.start()
        session.awaitLog("on", "prepare", "publish", "run")
        session.retireGate = CompletableDeferred()
        lifecycle.stop()
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire")

        lifecycle.start()
        lifecycle.stop()
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "on", "off")

        session.retireGate!!.complete(Unit)
        session.retireGate = null
        // The successor acquired nothing, so its own finalization retires nothing and only settles - after
        // the predecessor's, which is the ordering its re-join preserves.
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "on", "off", "settled")
        assertTrue(lifecycle.idle)
    }

    /**
     * A finished lifespan neither clears the intent of a start made while it was tearing down nor winds the
     * owner down under it.
     *
     * Mutation-sensitive on both identity guards - the session's own on `withdraw` and the owner's on the
     * final bookkeeping: drop either and the log gains an `off` or a `settled` that belongs to nobody.
     */
    @Test
    fun aStaleFinalizerCannotClearOrStopASuccessor() = driving { lifecycle, session ->
        lifecycle.start()
        session.awaitLog("on", "prepare", "publish", "run")
        session.retireGate = CompletableDeferred()
        lifecycle.stop()
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire")

        lifecycle.start()
        val successor = session.owner
        session.retireGate!!.complete(Unit)
        session.retireGate = null
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "on",
            "prepare", "publish", "run")
        assertEquals("the predecessor settled nothing on the successor's behalf", 0, session.settlements)
        assertEquals("and left its intent alone", listOf(true, false, true), session.intents)
        assertSame("which is still the successor's own", successor, session.owner)
        assertFalse(lifecycle.idle)
    }

    /**
     * A component Android destroyed mid-teardown is still the predecessor of the one it is replaced by.
     *
     * The two instances are the whole point: destruction leaves the destroyed instance's scope alive so its
     * finalizers can finish, so the recreated instance is a *different* [ShizukuLifecycle] on a *different*
     * scope, with an empty field of its own. What the two share is the process - here the one recorder, as
     * in production the one `ShizukuTestNetwork` - and the barrier has to be shared with it, or the
     * recreated start authorizes, retries the old finalizer's debt and acquires straight through a
     * withdrawal still in flight.
     *
     * The destroyed side is [ShizukuLifecycle.destroy] rather than a stop, which is what the production
     * `onDestroy` calls, and it leaves *two* lifespans behind on that scope: the cancelled one running its
     * own retirement, and the cleanup-only successor installed over it. The chain the recreated start joins
     * is therefore two links long, and only its far end settles the destroyed scope - which is what the
     * fence count and the settlement count below say together.
     *
     * Mutation-sensitive by construction: make the barrier a field of the instance again and the recreated
     * start's `prepare` appears below while the destroyed one is still gated inside `retire`.
     */
    @Test
    fun aRecreatedInstanceWaitsForTheDestroyedOnesFinalizer() = driving { destroyed, session ->
        destroyed.start()
        session.awaitLog("on", "prepare", "publish", "run")

        // Android removes the component while the session runs: a cleanup-only successor goes in over the
        // running lifespan, then intent goes off and that lifespan is cancelled into the teardown the gate
        // holds open.
        session.retireGate = CompletableDeferred()
        destroyed.destroy()
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire")

        // The replacement, built exactly as the service builds one: its own scope, its own lifecycle.
        val recreatedScope = CoroutineScope(coroutineContext + Dispatchers.Unconfined + SupervisorJob() +
                session.handler)
        try {
            val recreated = ShizukuLifecycle(recreatedScope, session)
            recreated.start()
            // Synchronously, because the body of a start runs inline up to its first suspension: with the
            // barrier that suspension is the join, and without it the log already reads "prepare" here.
            assertEquals("the recreated start is accepted and shown at once", listOf(true, false, true),
                session.intents)
            assertEquals("but prepares nothing behind a finalizer that has not finished", 1,
                session.log.count { it == "prepare" })
            assertFalse("and is in flight the whole time it waits", recreated.idle)
            assertFalse("while the instance it replaced still owns both of its lifespans", destroyed.idle)

            session.retireGate!!.complete(Unit)
            session.retireGate = null
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "on", "settled",
                "prepare", "publish", "run")
            assertEquals("both of the destroyed instance's lifespans reached their own bookkeeping", 2,
                session.fenceQueries)
            assertEquals("and exactly one of them settled that scope: the successor, never the lifespan it "
                + "replaced", 1, session.settlements)
            assertTrue("so it has nothing left in flight", destroyed.idle)
            assertFalse("and the replacement it never touched is running", recreated.idle)
        } finally {
            // Opened before this scope is cancelled, for the reason `driving` opens the same gates: a
            // lifespan re-joins its predecessor inside a [kotlinx.coroutines.NonCancellable] finalizer, so
            // cancelling one while that predecessor is still gated waits for something no cancellation can
            // release.
            session.retireGate?.complete(Unit)
            recreatedScope.coroutineContext.job.cancelAndJoin()
        }
    }

    /**
     * A finalizer asks for the withdrawal of its *own* lifespan, never of whichever one is accepted by then.
     *
     * This is the key the ledger's destructive entry point is guarded on. A generation belongs to the
     * lifespan that published it, so `ShizukuTestNetwork.stop` compares that owner against the one handed to
     * it and leaves anything else whole - which is only sound if what the finalizer hands it is itself. The
     * dangerous case is exactly this one: a replacement is already the accepted owner while the destroyed
     * instance's withdrawal is still in flight, so a teardown that named "the current session" instead would
     * be naming the replacement's.
     *
     * Destruction sharpens it rather than changing it. The production `onDestroy` leaves a cleanup-only
     * successor on the destroyed scope as well, and that successor settles the inherited ledger *unkeyed* -
     * so the thing it must never do is take the keyed withdrawal instead, under a lifespan that is not its
     * own and while a replacement is the accepted owner. The withdrawal count and `retiredBy` below say it
     * did not.
     *
     * Mutation-sensitive on that: pass the accepted owner rather than `self` and the two assertions inside
     * the gate swap answers; have the cleanup successor retire under itself and the count after the gate
     * reads two.
     */
    @Test
    fun aStaleFinalizerRetiresUnderItsOwnOwnerNotItsReplacements() = driving { destroyed, session ->
        destroyed.start()
        val destroyedOwner = session.owner
        session.awaitLog("on", "prepare", "publish", "run")
        session.retireGate = CompletableDeferred()
        destroyed.destroy()
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire")

        val recreatedScope = CoroutineScope(coroutineContext + Dispatchers.Unconfined + SupervisorJob() +
                session.handler)
        try {
            val recreated = ShizukuLifecycle(recreatedScope, session)
            recreated.start()
            val recreatedOwner = session.owner
            assertNotSame("the replacement is a lifespan of its own", destroyedOwner, recreatedOwner)
            assertSame("the withdrawal still in flight is the destroyed lifespan's own", destroyedOwner,
                session.retiredBy)

            session.retireGate!!.complete(Unit)
            session.retireGate = null
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "on", "settled",
                "prepare", "publish", "run")
            assertSame("and finishing changed nothing about whose it was", destroyedOwner,
                session.retiredBy)
            assertEquals("the cleanup successor behind it withdrew nothing of its own", 1,
                session.log.count { it == "retire" })

            // The replacement's own teardown is the other half: it names itself, not its predecessor.
            recreated.stop()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "on", "settled",
                "prepare", "publish", "run", "off", "retire", "settled")
            assertSame("a lifespan retires under itself, whichever one it is", recreatedOwner,
                session.retiredBy)
            assertTrue(recreated.idle)
        } finally {
            session.retireGate?.complete(Unit)
            recreatedScope.coroutineContext.job.cancelAndJoin()
        }
    }

    /**
     * A component recreated behind a finalizer that is still running decides nothing about itself until
     * that finalizer has finished.
     *
     * The other recreation schedule, and the one an ordinary start does not cover: what the replacement
     * receives is an *idle stop*, so it installs no session and has no lifespan of its own to wind it down.
     * Asking the ledger there and then is what strands it - a withdrawal in flight still owns every resource
     * it is in the middle of releasing, so the answer is "outstanding", and the instance that will change
     * that answer settles only its own scope when it gets there. The replacement would keep itself
     * foreground for a process that has nothing left to give it.
     *
     * So the idle stop installs a cleanup-only lifespan, which takes the same process barrier a start takes.
     * With the destroyed side going through [ShizukuLifecycle.destroy], as production's `onDestroy` does,
     * that barrier is three links long - the replacement's housekeeping behind the destruction's own
     * successor behind the cancelled lifespan - and every one of them has to be waited out before the
     * replacement may decide anything about itself.
     *
     * Mutation-sensitive on exactly that join: drop it and the two assertions inside the gate both change -
     * the replacement queries the ledger and settles itself unfenced while a predecessor is still holding
     * the resources.
     */
    @Test
    fun aRecreatedIdleInstanceDecidesNothingUntilItsPredecessorFinished() = driving { destroyed, session ->
        destroyed.start()
        session.awaitLog("on", "prepare", "publish", "run")

        // Android removes the component mid-session: a cleanup-only successor goes in first, then intent
        // goes off and the running lifespan is cancelled into the teardown the gate holds open.
        session.retireGate = CompletableDeferred()
        destroyed.destroy()
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire")
        assertTrue("the ledger owes the session's resources for as long as that runs", session.debt)

        val recreatedScope = CoroutineScope(coroutineContext + Dispatchers.Unconfined + SupervisorJob() +
                session.handler)
        try {
            val recreated = ShizukuLifecycle(recreatedScope, session)
            // The idle stop delivered to the replacement: it installs no session, so this is all it gets.
            recreated.housekeep()
            assertFalse("which is in flight from the moment it is installed", recreated.idle)
            assertEquals("it asks the ledger nothing behind a finalizer that has not finished", 0,
                session.fenceQueries)
            assertEquals("and attempts nothing on top of one", 0, session.log.count { it == "settle" })
            assertEquals("so it has made no decision about itself", 0, session.settlements)
            assertEquals("and published nothing either", listOf(true, false), session.intents)
            assertFalse("while the instance it replaced still owns the teardown", destroyed.idle)

            session.retireGate!!.complete(Unit)
            session.retireGate = null
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "settled", "settled")
            assertEquals("all three lifespans reached their own bookkeeping, in order", 3,
                session.fenceQueries)
            assertEquals("the destroyed instance settled its own scope, then the replacement its own", 2,
                session.settlements)
            assertEquals("nothing was owed by the time either of them looked", 0,
                session.log.count { it == "settle" })
            assertEquals("which wound down on what its predecessor had actually fenced", true,
                session.fenced)
            assertTrue(destroyed.idle)
            assertTrue(recreated.idle)
        } finally {
            // Opened before this scope is cancelled, for the reason `driving` opens the same gates: a
            // lifespan joins its predecessor inside a [kotlinx.coroutines.NonCancellable] finalizer, so
            // cancelling one while that predecessor is still gated waits for something no cancellation can
            // release.
            session.retireGate?.complete(Unit)
            recreatedScope.coroutineContext.job.cancelAndJoin()
        }
    }

    /**
     * A stop during the *publication* half is immediate too, cancels it, and retires what it had reached.
     *
     * The other half of the cancellable contract: gating `prepare` only proves a lifespan that owns nothing
     * can be dropped, while this one is cancelled after it has begun acquiring - which is exactly when a
     * partially built generation exists and exactly once is what it may be rolled back.
     */
    @Test
    fun aStopDuringPublicationCancelsItAndRetiresOnce() = driving { lifecycle, session ->
        session.publishGate = CompletableDeferred()
        lifecycle.start()
        session.awaitLog("on", "prepare", "publish")

        lifecycle.stop()
        assertEquals("off the moment it is asked for", listOf(true, false), session.intents)

        session.publishGate!!.complete(Unit)
        session.publishGate = null
        session.awaitLog("on", "prepare", "publish", "off", "retire", "settled")
        assertEquals("exactly one rollback", 1, session.log.count { it == "retire" })
        assertTrue(lifecycle.idle)
    }

    /**
     * A reporter that throws cannot skip the teardown, the fence query, the job's clearing or the
     * settlement - in any of the three places the owner reports from.
     *
     * Two of them are inside the finalizer, and the third is not: a startup or a running session that failed
     * is reported from the `catch` *before* the `finally` is entered at all, so a reporter that throws there
     * would carry the lifespan straight out of the body with nothing yet withdrawn. That is the case the
     * outer `finally` exists for. All three are arranged deliberately rather than left to arrive as an
     * unexplained stack trace: the reporter's own failure is captured by the harness and asserted on, so a
     * green run stays quiet.
     */
    @Test
    fun aFailedReportCannotSkipCleanupOrBookkeeping() {
        val reporterDied = IllegalStateException("the reporter is gone")

        // (a) The session's own publication failed and reporting *that* failed, before the finalizer began.
        driving(Recorder(publishFails = IOException("agent refused"), reportFails = reporterDied)) {
                lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "off", "retire", "settled")
            assertEquals("the publication failure was reported before the reporter itself failed", 1,
                session.reported.size)
            assertEquals("the partly built generation was still withdrawn", 1,
                session.log.count { it == "retire" })
            assertEquals("the ledger was still asked", 1, session.fenceQueries)
            assertEquals(true, session.fenced)
            assertEquals("settled exactly once", 1, session.settlements)
            assertTrue("and the job was cleared", lifecycle.idle)
            assertSame("the reporter's own failure left the lifespan, and only it", reporterDied,
                session.takeUncaught().single())
        }

        // (b) The withdrawal failed and reporting *that* failed too.
        driving(Recorder(withdrawal = Recorder.Withdrawal.LeavesLocalDebt(IOException("child survived")),
            reportFails = reporterDied)) {
                lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run")
            lifecycle.stop()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced")
            assertEquals("the failure was reported before the reporter itself failed", 1,
                session.reported.size)
            assertEquals("the ledger was still asked", 1, session.fenceQueries)
            assertEquals("and answered from the debt the failed withdrawal left", false, session.fenced)
            assertEquals("settled exactly once", 1, session.settlements)
            assertTrue("and the job was cleared", lifecycle.idle)
            assertSame("the reporter's own failure left the lifespan, and only it", reporterDied,
                session.takeUncaught().single())
        }

        // (c) The withdrawal succeeded, the *fence query* failed, and reporting that failed too.
        driving(Recorder(reportFails = reporterDied).apply { fenceFails = IOException("the lane is gone") }) {
                lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run")
            lifecycle.stop()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced")
            assertEquals("the query's failure was reported", 1, session.reported.size)
            assertEquals("a ledger that could not be reached is not one that said nothing is owed", false,
                session.fenced)
            assertEquals("settled exactly once", 1, session.settlements)
            assertTrue("and the job was cleared", lifecycle.idle)
            assertSame("the reporter's own failure left the lifespan, and only it", reporterDied,
                session.takeUncaught().single())
        }
    }

    /** A publication that failed is rolled back exactly once, by the lifespan's own finalizer. */
    @Test
    fun aPublicationFailureRollsBackExactlyOnce() =
        driving(Recorder(publishFails = IOException("agent refused"))) { lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "off", "retire", "settled")
            assertEquals("the failure is reported rather than swallowed", 1, session.reported.size)
            assertTrue("${session.reported}", session.reported.single() is IOException)
            assertEquals(true, session.fenced)
            assertTrue(lifecycle.idle)
        }

    /**
     * A teardown that could not fence its resources leaves *no* job behind, and says so through the
     * settlement instead.
     *
     * This is the separation the owner model rests on. The job means work in flight and nothing else, so it
     * is gone; what remains is concrete ledger debt, which reaches the foreground owner as `settled(false)`
     * and is readable afterwards by asking the ledger again - which is what the cleanup-only lifespan a
     * later idle command installs does, before it *attempts* that same debt rather than only reporting it.
     * Mutation-sensitive on the clearing being unconditional: keep the completed job and `idle` below is
     * false.
     */
    @Test
    fun anUnfencedTeardownLeavesDebtInTheLedgerAndNoJob() =
        driving(Recorder(withdrawal =
            Recorder.Withdrawal.LeavesLocalDebt(IllegalStateException("child survived")))) {
                lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run")
            lifecycle.stop()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced")
            assertEquals("the ledger, not the owner, is what remembers the debt", false, session.fenced)
            assertEquals("asked of the ledger once the teardown was over", 1, session.fenceQueries)
            assertTrue("nothing is in flight, so nothing is installed", lifecycle.idle)
            assertEquals(1, session.reported.size)

            // A stop with nothing in flight publishes nothing and cancels nothing, and the ledger still says
            // the debt stands - which is what the foreground owner has to hear before it decides it may go.
            lifecycle.stop()
            assertEquals("a stop with nothing in flight is a no-op", listOf(true, false), session.intents)
            assertEquals("and settles nothing a second time", 1, session.settlements)
            assertTrue(lifecycle.idle)
            assertFalse("so the component is kept over the debt", session.fenced())
            assertEquals("by one question, not a loop", 2, session.fenceQueries)

            // A later start is the next attempt, and it discharges the debt once, before it publishes.
            session.withdrawal = Recorder.Withdrawal.Fences
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced",
                "on", "settle", "prepare", "publish", "run")
            assertEquals("one start, one attempt", 1, session.log.count { it == "settle" })
            assertTrue("settled before this start asked anything of its own",
                session.log.indexOf("settle") < session.log.lastIndexOf("prepare"))

            lifecycle.stop()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced",
                "on", "settle", "prepare", "publish", "run", "off", "retire", "settled")
            assertEquals("and nothing is owed once it is over", true, session.fenced)
        }

    /**
     * Debt a teardown could not fence is still owned once the component is being destroyed, and gets an
     * attempt rather than being abandoned.
     *
     * The state this covers is the one the test above leaves behind: nothing in flight, and a ledger that
     * still names a child, a descriptor or an agent. There is no lifespan left to retry it and no start on
     * the way, so an idle stop - or Android removing the component, which does not remove the process an
     * activity or another foreground service may be holding up - would drop it with no owner at all.
     *
     * Mutation-sensitive twice over: give the command no lifespan and nothing below runs; give it one that
     * only queries the ledger rather than settling it, and the second settlement reads unfenced over a debt
     * that was recoverable all along.
     */
    @Test
    fun aDestroyedIdleInstanceStillOwnsRecoverableDebt() =
        driving(Recorder(withdrawal =
            Recorder.Withdrawal.LeavesLocalDebt(IllegalStateException("child survived")))) {
                lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run")
            lifecycle.stop()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced")
            assertTrue("no lifespan is left that would retry it", lifecycle.idle)
            assertEquals("and the ledger says the debt stands", false, session.fenced)

            // The command with no session to install: an idle stop, or the component being destroyed.
            session.settleGate = CompletableDeferred()
            lifecycle.housekeep()
            assertFalse("the debt has an owner again", lifecycle.idle)
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced", "settle")
            assertEquals("one that publishes nothing", listOf(true, false), session.intents)
            assertEquals("and acquires nothing", 1, session.log.count { it == "publish" })
            assertEquals("nothing has settled again while its attempt is still in flight", 1,
                session.settlements)

            session.settleGate!!.complete(Unit)
            session.settleGate = null
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced", "settle",
                "settled")
            assertEquals("the attempt discharged it, so the component may go after all", true,
                session.fenced)
            assertEquals("by one attempt and one settlement, not a loop", 2, session.settlements)
            assertEquals(1, session.log.count { it == "settle" })
            assertTrue(lifecycle.idle)
        }

    /**
     * A housekeeping lifespan is a predecessor like any other, which is the whole of what makes the cleanup
     * it runs safe to be unkeyed.
     *
     * `ShizukuTestNetwork.settle` names no owner, and cannot: inherited debt is by definition a generation
     * some *other* lifespan published, so no token this one holds could name it. What stops it reaching a
     * newer generation instead is that none can exist while it runs - it takes the process barrier, so a
     * start from any instance orders itself behind it exactly as it would behind a session's own teardown,
     * and the one attempt stays one.
     *
     * Mutation-sensitive on the cleanup lifespan taking that barrier rather than only this instance's
     * liveness: leave it out and the start below runs the *same* attempt a second time, concurrently with
     * the one already in flight.
     */
    @Test
    fun aHousekeepingLifespanIsAPredecessorLikeAnyOther() =
        driving(Recorder(withdrawal =
            Recorder.Withdrawal.LeavesLocalDebt(IllegalStateException("child survived")))) {
                lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run")
            lifecycle.stop()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced")

            session.settleGate = CompletableDeferred()
            lifecycle.housekeep()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced", "settle")

            lifecycle.start()
            assertEquals("the start is accepted and shown at once", listOf(true, false, true),
                session.intents)
            assertEquals("but attempts nothing behind a cleanup that has not finished", 1,
                session.log.count { it == "settle" })
            assertEquals("and prepares nothing either", 1, session.log.count { it == "prepare" })

            session.settleGate!!.complete(Unit)
            session.settleGate = null
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced", "settle",
                "on", "prepare", "publish", "run")
            assertEquals("the debt was settled once, by the lifespan that owned the attempt", 1,
                session.log.count { it == "settle" })
            assertEquals("and the superseded cleanup settled nothing on the successor's behalf", 1,
                session.settlements)
            assertFalse(lifecycle.idle)
        }

    /**
     * A destruction owns the last attempt at what the lifespan it cancelled could not fence.
     *
     * The schedule an idle destruction does not reach: the failure happens *under* the destruction rather
     * than before it. `onDestroy` cancels a live lifespan, that lifespan's own withdrawal cannot fence a
     * child, a descriptor or an agent, and its final bookkeeping would then settle unfenced with the
     * component already gone - which ends the scope and abandons recoverable debt in a process an activity
     * or another foreground service may well still be holding up. There is no later command to retry it,
     * because there is no longer a component to deliver one to.
     *
     * So destruction installs a cleanup-only successor *over* the lifespan it cancels, and the ordering is
     * what this proves. The cancelled lifespan is deliberately left ungated here, so its whole finalizer
     * runs inline inside the destruction: install it after the cancellation instead and that finalizer is
     * still the installed one when it settles, so it ends the scope first and the assertion on settlements
     * below fails at once. Drop its successor's join and the retry appears in the log ahead of the
     * withdrawal it is supposed to be resuming.
     */
    @Test
    fun destructionOwnsTheLastAttemptAtWhatItCancelledAndCouldNotFence() =
        driving(Recorder(withdrawal =
            Recorder.Withdrawal.LeavesLocalDebt(IllegalStateException("child survived")))) {
                lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run")

            // Held so the successor's one attempt is still visibly in flight when this looks at it.
            session.settleGate = CompletableDeferred()
            lifecycle.destroy()
            assertEquals("intent goes off with the component", listOf(true, false), session.intents)
            assertEquals("and the lifespan it cancelled settled nothing on its successor's behalf", 0,
                session.settlements)
            assertFalse("which is a lifespan of this instance's, so the owner still has exactly one",
                lifecycle.idle)

            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "settle")
            assertTrue("the withdrawal that failed ran first, and the retry only behind it",
                session.log.indexOf("retire") < session.log.indexOf("settle"))
            assertEquals("nothing published or acquired anything on the way", 1,
                session.log.count { it == "publish" })

            session.settleGate!!.complete(Unit)
            session.settleGate = null
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "settle", "settled")
            assertEquals("exactly one lifespan settled this owner: the successor, after its own attempt", 1,
                session.settlements)
            assertEquals("which fenced what the cancelled one could not", true, session.fenced)
            assertEquals("by one retry, not two", 1, session.log.count { it == "settle" })
            assertEquals("and one report, from the withdrawal that actually failed", 1,
                session.reported.size)
            assertTrue(lifecycle.idle)
        }

    /**
     * Inherited cleanup runs in front of every successor-only prerequisite, so a gate that refuses the new
     * session cannot also refuse the old one its cleanup.
     *
     * `prepare` stands for all of them here - a Shizuku authorization the user revoked, a device that will
     * not consult the preference, a tethering service that died and cannot be brought back - and any of them
     * can be permanent, which is what makes the ordering the whole of the fix rather than a tidiness: behind
     * such a gate a recoverable child, descriptor or agent would have no retry left at all. None of them is
     * what a withdrawal was missing; local fencing asks for none of what they ask for.
     *
     * Mutation-sensitive on the order: settle after `prepare` and no `settle` appears below at all, because
     * the gate throws first - and the debt is still standing when the lifespan settles.
     */
    @Test
    fun inheritedDebtIsSettledAheadOfAFailingSuccessorGate() =
        driving(Recorder(withdrawal =
            Recorder.Withdrawal.LeavesLocalDebt(IllegalStateException("child survived")))) {
                lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run")
            lifecycle.stop()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced")
            assertEquals(false, session.fenced)

            // Shizuku is gone by the time the user tries again, and it is not coming back.
            session.prepareFails = IOException("Shizuku is no longer authorized")
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced",
                "on", "settle", "prepare", "off", "settled")
            assertEquals(2, session.reported.size)
            assertTrue("the new session was refused: ${session.reported}",
                session.reported.last() is IOException)
            assertEquals("and nothing was published over what the ledger still owed", 1,
                session.log.count { it == "publish" })
            assertEquals("but the debt it inherited was fenced first, by one attempt", 1,
                session.log.count { it == "settle" })
            assertEquals("so the component is released rather than held over it", true, session.fenced)
            assertTrue(lifecycle.idle)
        }

    /**
     * The ledger is asked after the teardown, not beside it.
     *
     * Mutation-sensitive on where the query sits: ask before [ShizukuLifecycle.Session.retire] and the
     * settlement below completes while the withdrawal is still gated open.
     */
    @Test
    fun theLedgerIsAskedOnlyAfterTheTeardownFinished() = driving { lifecycle, session ->
        session.retireGate = CompletableDeferred()
        lifecycle.start()
        session.awaitLog("on", "prepare", "publish", "run")
        lifecycle.stop()
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire")
        assertEquals("not while the withdrawal is still in flight", 0, session.fenceQueries)
        assertEquals(0, session.settlements)
        session.retireGate!!.complete(Unit)
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "settled")
        assertEquals(1, session.fenceQueries)
    }

    /**
     * One start makes one cleanup attempt, and never publishes over what that attempt left.
     *
     * A lifespan whose `settle` failed on its *predecessor's* debt acquired nothing of its own, so its
     * finalizer must not immediately attempt that same debt a second time - and it must not have got as far
     * as `prepare` either, because a generation the ledger still owes is exactly what forbids a successor.
     * Mutation-sensitive on both: settle unconditionally in the finalizer and a second `settle` appears
     * below, and let a failed settlement fall through and a `prepare` appears after it.
     */
    @Test
    fun predecessorDebtIsNotRetriedTwiceByOneStart() =
        driving(Recorder(withdrawal =
            Recorder.Withdrawal.LeavesLocalDebt(IllegalStateException("child survived")))) {
                lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run")
            lifecycle.stop()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced")

            // The successor's own settlement is what attempts that debt, and here the attempt fails.
            session.settleFails = IOException("the debt is still outstanding")
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced",
                "on", "settle", "off", "unfenced")
            assertEquals("one start, one attempt at the debt", 1, session.log.count { it == "settle" })
            assertEquals("and no second attempt from its finalizer", 1,
                session.log.count { it == "retire" })
            assertEquals("nothing was prepared over a generation the ledger still owes", 1,
                session.log.count { it == "prepare" })
            assertEquals("and nothing was published over it", 1, session.log.count { it == "publish" })
            assertEquals("the debt outlived the failed attempt, so the owner still holds", false,
                session.fenced)
            assertEquals(2, session.reported.size)

            // A *later* start is the next attempt, and this one settles it.
            session.settleFails = null
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced",
                "on", "settle", "off", "unfenced",
                "on", "settle", "prepare", "publish", "run")
            assertEquals("two starts, two attempts, never two in one", 2,
                session.log.count { it == "settle" })
            assertEquals(1, session.log.count { it == "retire" })
        }

    /**
     * A withdrawal can fail without leaving anything local owed.
     *
     * Local fencing - the child, the agent, the descriptor - all runs before the privileged release, so a
     * release that could not be confirmed is a real failure the user is told about while the ledger
     * correctly says nothing local is left. Mutation-sensitive on the settlement being the *ledger's*
     * answer: derive `fenced` from whether the withdrawal threw and this settles unfenced and holds the
     * component over a debt that does not exist.
     */
    @Test
    fun aResidualWithdrawalIsReportedAndStillSettlesFenced() =
        driving(Recorder(withdrawal = Recorder.Withdrawal.LeavesResidual(
            IOException("the privileged release was not confirmed")))) { lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run")
            lifecycle.stop()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "settled")
            assertEquals("reported once, by the one layer that asked for the withdrawal", 1,
                session.reported.size)
            assertEquals("the ledger was asked rather than inferred from the failure", 1,
                session.fenceQueries)
            assertEquals(true, session.fenced)
            assertEquals(1, session.settlements)
            assertTrue("so the job is cleared and the owner may let go", lifecycle.idle)
        }

    /**
     * A predecessor finalizing after a successor was accepted cannot turn the row off under it.
     *
     * The production identity guard, called directly on the singleton that owns intent rather than through
     * the fake: `withdrawIntent` is a compare-and-set on owner identity, and that is the whole reason the
     * lifespan finalizer may withdraw unconditionally. Process-wide state, so it is restored on the way out.
     */
    @Test
    fun aStaleOwnerCannotWithdrawItsSuccessorsIntent() {
        val restore = ShizukuTestNetwork.intent.value
        val predecessor = Job()
        val successor = Job()
        try {
            ShizukuTestNetwork.publishIntent(predecessor)
            assertSame(predecessor, ShizukuTestNetwork.intent.value)

            // Accepted while the predecessor is still finalizing, which is the ordinary restart.
            ShizukuTestNetwork.publishIntent(successor)
            ShizukuTestNetwork.withdrawIntent(predecessor)
            assertSame("the predecessor's own withdrawal is a no-op once it is not the accepted owner",
                successor, ShizukuTestNetwork.intent.value)

            ShizukuTestNetwork.withdrawIntent(successor)
            assertNull("only its own owner clears it", ShizukuTestNetwork.intent.value)
            ShizukuTestNetwork.withdrawIntent(successor)
            assertNull("and withdrawing twice changes nothing", ShizukuTestNetwork.intent.value)
        } finally {
            ShizukuTestNetwork.intent.value?.let { ShizukuTestNetwork.withdrawIntent(it) }
            restore?.let { ShizukuTestNetwork.publishIntent(it) }
        }
    }

    /**
     * A committed state can only ever label the lifespan that produced it.
     *
     * The production rule, called directly. Publication onto the display flow is not cancellable, so an
     * observer whose lifespan was cancelled - and which therefore stopped being the accepted owner - can
     * still land a write after a successor was accepted. No ordering rule makes that impossible, and no
     * liveness check answers it either, because liveness is not identity: it cannot say whose label a value
     * is. Only the stamp makes it harmless. No dispatcher, no clock and no framework is needed to state
     * that.
     */
    @Test
    fun aStaleOwnersCommittedStateNeverLabelsASuccessor() {
        val predecessor = Job()
        val successor = Job()
        val late = OwnedState(predecessor, ShizukuTestNetwork.State.ACTIVE)
        assertNull("a predecessor's late write cannot label the lifespan that replaced it",
            OwnedState.label(successor, late))
        assertNull("nor can anything label an intent that is off", OwnedState.label(null, late))
        assertEquals("while its own owner still shows it", ShizukuTestNetwork.State.ACTIVE.label,
            OwnedState.label(predecessor, late))

        val committed = OwnedState(successor, ShizukuTestNetwork.State.ARMED)
        assertEquals("and the successor's own state is what the row then reads",
            ShizukuTestNetwork.State.ARMED.label, OwnedState.label(successor, committed))
        assertNull("a successor that has committed nothing yet carries no label",
            OwnedState.label(successor, null))
    }
}
