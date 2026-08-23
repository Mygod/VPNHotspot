package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.yield
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.IOException
import java.util.concurrent.atomic.AtomicInteger

/**
 * The whole of what this mode's command lane promises: one session at a time, one command at a time, and a
 * state that never says off while something of a session is still there.
 *
 * Deliberately short, and deliberately Shizuku-only. Nothing here has an opinion about root mode, because the
 * lane has none: it serializes this mode's own resources and reports this mode's own state, and no transition
 * it drives touches root routing.
 */
class ShizukuLifecycleTest {
    /** The one side, recorded in order so a test can assert *when* something ran, not just whether. */
    private class Recorder(
        val prepareFails: Throwable? = null,
        val publishFails: Throwable? = null,
        var retireFails: Throwable? = null,
    ) : ShizukuLifecycle.Session {
        val log = mutableListOf<String>()
        val prepares = AtomicInteger()

        /** Held open by a test that needs to observe [ShizukuLifecycle.State.PREPARING] from outside. */
        var gate: CompletableDeferred<Unit>? = null

        override suspend fun prepare(): suspend () -> Unit {
            prepares.incrementAndGet()
            log += "prepare"
            gate?.await()
            prepareFails?.let { throw it }
            return {
                log += "publish"
                publishFails?.let { throw it }
            }
        }

        override suspend fun retire() {
            log += "retire"
            retireFails?.let { throw it }
        }
    }

    /**
     * The lane's flights run on `runBlocking`'s event loop, so a start, its caller and the opposite command
     * share one thread and the orderings asserted below are the real ones.
     */
    private fun driving(block: suspend CoroutineScope.(ShizukuLifecycle, Recorder) -> Unit) = runBlocking {
        val session = Recorder()
        block(ShizukuLifecycle(session), session)
    }

    @Test
    fun aStartPreparesThenPublishes() = driving { lifecycle, session ->
        lifecycle.start()
        assertEquals(ShizukuLifecycle.State.ON, lifecycle.state.value)
        assertEquals(listOf("prepare", "publish"), session.log)
    }

    /**
     * A start that fails before anything is created reports off and retires nothing.
     *
     * Nothing exists to withdraw at that point - preparation is the half that mutates nothing - so asking for
     * a retirement would be asking a session that was never published to take itself back.
     */
    @Test
    fun aPreparationFailureRetiresNothing() {
        val session = Recorder(prepareFails = IOException("no Shizuku"))
        val lifecycle = ShizukuLifecycle(session)
        val failure = runCatchingFailure { lifecycle.start() }
        assertTrue("$failure", failure is IOException)
        assertEquals(ShizukuLifecycle.State.OFF, lifecycle.state.value)
        assertEquals(listOf("prepare"), session.log)
    }

    /**
     * A publication that failed is retired exactly once, before the failure is reported, and the mode reads
     * off afterwards.
     *
     * Once is the assertion that matters. Publication creates the TUN, the request, the agent, the
     * preference and the child one at a time, so a failure halfway leaves some of them behind and only
     * [ShizukuLifecycle.Session.retire] knows which - and it is this lane that runs it. A publication that
     * also retired itself would make the log below `retire, retire`, which is a second long withdrawal
     * racing the first and, when the first fails, an immediate retry of a teardown the next explicit stop
     * is supposed to own.
     */
    @Test
    fun aPublicationFailureRetiresWhatItCreatedExactlyOnce() {
        val session = Recorder(publishFails = IOException("agent refused"))
        val lifecycle = ShizukuLifecycle(session)
        val failure = runCatchingFailure { lifecycle.start() }
        assertTrue("$failure", failure is IOException)
        assertEquals(listOf("prepare", "publish", "retire"), session.log)
        assertEquals(ShizukuLifecycle.State.OFF, lifecycle.state.value)
    }

    /**
     * A rollback that could not finish keeps the mode reported as on, with both causes attached.
     *
     * Fail closed: something of the session may still be relaying, so the row has to keep offering the stop
     * that retries it. Reporting off here would hide a live child behind a control that says there is none.
     */
    @Test
    fun aRollbackThatFailedKeepsTheModeOn() {
        val session = Recorder(publishFails = IOException("agent refused"),
            retireFails = IllegalStateException("child survived"))
        val lifecycle = ShizukuLifecycle(session)
        val failure = runCatchingFailure { lifecycle.start() }
        assertTrue("$failure", failure is IOException)
        assertEquals("the rollback failure travels attached to the startup one", 1,
            failure!!.attachments().count { it is IllegalStateException })
        assertEquals(ShizukuLifecycle.State.ON, lifecycle.state.value)

        // And the next stop is a retry rather than a fresh start, which is what that state is for.
        session.retireFails = null
        runBlocking { lifecycle.stop() }
        assertEquals(listOf("prepare", "publish", "retire", "retire"), session.log)
        assertEquals(ShizukuLifecycle.State.OFF, lifecycle.state.value)
    }

    /**
     * A retirement that fails with a [CancellationException] is treated exactly like any other failure.
     *
     * The rollback runs inside [kotlinx.coroutines.NonCancellable], so a cancellation arriving from it is
     * never this caller going away: it is a deadline or a replaced Shizuku epoch, which are the two ways the
     * real session reports that it could not confirm a resource is gone. Rethrowing it - as an earlier
     * revision did - reported the mode as off over a child that may still be relaying, and lost the
     * publication failure the user actually needed to see.
     *
     * Mutation-sensitive on that branch: special-case the cancellation again and both assertions below fail.
     */
    @Test
    fun aRetirementCancellationIsAFailureLikeAnyOther() {
        val session = Recorder(publishFails = IOException("agent refused"),
            retireFails = RetirementDeadline())
        val lifecycle = ShizukuLifecycle(session)
        val failure = runCatchingFailure { lifecycle.start() }
        assertTrue("$failure", failure is IOException)
        assertEquals("the deadline travels attached to the startup failure", 1,
            failure!!.attachments().count { it is RetirementDeadline })
        assertEquals(ShizukuLifecycle.State.ON, lifecycle.state.value)
    }

    /** A stop with nothing running does nothing at all, and does not report a failure for it. */
    @Test
    fun aStopWithNothingRunningIsANoOp() = driving { lifecycle, session ->
        lifecycle.stop()
        assertEquals(ShizukuLifecycle.State.OFF, lifecycle.state.value)
        assertTrue(session.log.isEmpty())
    }

    @Test
    fun aStopRetiresTheSession() = driving { lifecycle, session ->
        lifecycle.start()
        lifecycle.stop()
        assertEquals(ShizukuLifecycle.State.OFF, lifecycle.state.value)
        assertEquals(listOf("prepare", "publish", "retire"), session.log)
    }

    /**
     * A stop that could not confirm the session is gone leaves the mode on and reports why.
     *
     * Same rule as the failed rollback, from the other direction: the child may still be relaying, so saying
     * the mode is off would be a lie and would take away the only control that can retry.
     */
    @Test
    fun aStopThatCouldNotConfirmKeepsTheModeOn() {
        val session = Recorder(retireFails = IllegalStateException("child survived"))
        val lifecycle = ShizukuLifecycle(session)
        runBlocking { lifecycle.start() }
        val failure = runCatchingFailure { lifecycle.stop() }
        assertTrue("$failure", failure is IllegalStateException)
        assertEquals(ShizukuLifecycle.State.ON, lifecycle.state.value)
    }

    /**
     * Two starts pressed at once are one start, and both callers get its outcome.
     *
     * A second flight would acquire a second TUN, a second agent and a second child behind the one session
     * ledger, which is exactly what the lane exists to make unrepresentable.
     */
    @Test
    fun aDuplicateStartSharesTheOneInFlight() = driving { lifecycle, session ->
        session.gate = CompletableDeferred()
        val first = launch { lifecycle.start() }
        val second = launch { lifecycle.start() }
        yield()
        assertEquals(ShizukuLifecycle.State.PREPARING, lifecycle.state.value)
        session.gate!!.complete(Unit)
        first.join()
        second.join()
        assertEquals("one flight, however many presses", 1, session.prepares.get())
        assertEquals(listOf("prepare", "publish"), session.log)
        assertEquals(ShizukuLifecycle.State.ON, lifecycle.state.value)
    }

    /** A start pressed while the mode is already on is not a second publication. */
    @Test
    fun aStartWhileAlreadyOnDoesNothing() = driving { lifecycle, session ->
        lifecycle.start()
        lifecycle.start()
        assertEquals(1, session.prepares.get())
        assertEquals(listOf("prepare", "publish"), session.log)
    }

    /**
     * A stop arriving while a start is still waiting on the user supersedes it, nothing is ever created, and
     * the mode reports the stop as in progress until that start has settled.
     *
     * The interactive half can sit on a Shizuku permission dialog for as long as the user takes. A stop that
     * queued behind it would publish a whole session and then immediately tear it down; a stop that cancelled
     * the caller's coroutine would leave the lane guessing. The generation check does neither.
     *
     * The window afterwards is the bug this also covers. Publishing [ShizukuLifecycle.State.OFF] the moment
     * the stop was recorded left the doomed flight installed with nothing saying so, and a start arriving in
     * that window joined it and inherited its [ShizukuLifecycle.SupersededException] instead of running.
     * Reporting the supersession as a stop in progress - which is also what the user just asked for - is
     * what closes it, and only the leader may publish off.
     *
     * Mutation-sensitive on the first block: set `OFF` in the `PREPARING` arm of `stop` and every assertion
     * straight after the stop fails. That the flight and the state it describes are retired in *one* lane
     * section is structural rather than observable from a test, so what the last block shows instead is the
     * property a caller depends on - once off is published there is no flight left to join, and the next
     * start really is a second preparation.
     */
    @Test
    fun aStopDuringPreparationStaysBusyUntilTheLeaderSettles() = driving { lifecycle, session ->
        session.gate = CompletableDeferred()
        var failure: Throwable? = null
        val start = launch {
            failure = runCatchingSuspend { lifecycle.start() }
        }
        yield()
        assertEquals(ShizukuLifecycle.State.PREPARING, lifecycle.state.value)

        lifecycle.stop()
        assertEquals("the leader has not settled, so this is a stop in progress",
            ShizukuLifecycle.State.RETIRING, lifecycle.state.value)
        assertTrue("and the row keeps offering neither command", lifecycle.state.value.busy)
        assertTrue(lifecycle.state.value.on)
        assertEquals("nothing was created, so nothing was retired", listOf("prepare"), session.log)

        // A second press while the first has not landed is not a second supersession.
        lifecycle.stop()
        assertEquals(ShizukuLifecycle.State.RETIRING, lifecycle.state.value)
        assertEquals(listOf("prepare"), session.log)

        // Only the leader publishes off, and it does so with the flight already gone.
        session.gate!!.complete(Unit)
        start.join()
        assertTrue("$failure", failure is ShizukuLifecycle.SupersededException)
        assertFalse("a superseded start must never publish", session.log.contains("publish"))
        assertEquals(ShizukuLifecycle.State.OFF, lifecycle.state.value)

        // No stale flight is left behind: this is a fresh start, not a share of the superseded one.
        session.gate = null
        lifecycle.start()
        assertEquals(ShizukuLifecycle.State.ON, lifecycle.state.value)
        assertEquals(listOf("prepare", "prepare", "publish"), session.log)
        assertEquals(2, session.prepares.get())
    }

    /**
     * A start pressed again after a stop superseded one is a real start, not a second supersession.
     *
     * Mutation-sensitive on the generation: sample it after the interactive half instead of before, and this
     * start would inherit the earlier stop and refuse to publish.
     */
    @Test
    fun aStartAfterASupersessionStillPublishes() = driving { lifecycle, session ->
        session.gate = CompletableDeferred()
        val superseded = launch { runCatchingSuspend { lifecycle.start() } }
        yield()
        lifecycle.stop()
        session.gate!!.complete(Unit)
        superseded.join()

        session.gate = null
        lifecycle.start()
        assertEquals(ShizukuLifecycle.State.ON, lifecycle.state.value)
        assertEquals(listOf("prepare", "prepare", "publish"), session.log)
    }

    /**
     * A stop pressed during a publication waits for it and then retires it, rather than interleaving.
     *
     * Publication is one rollback boundary: a stop that ran through the middle of it would release resources
     * the start has not finished recording, and leak whatever it had not reached yet.
     */
    @Test
    fun aStopDuringPublicationRunsAfterIt() = driving { lifecycle, session ->
        val gate = CompletableDeferred<Unit>()
        val slow = object : ShizukuLifecycle.Session by session {
            override suspend fun prepare(): suspend () -> Unit {
                session.prepare()
                return {
                    gate.await()
                    session.log += "publish"
                }
            }
        }
        val lifecycle = ShizukuLifecycle(slow)
        val start = launch { lifecycle.start() }
        yield()
        val stop = launch { lifecycle.stop() }
        yield()
        assertFalse("the stop must not run inside the publication", session.log.contains("retire"))
        gate.complete(Unit)
        start.join()
        stop.join()
        assertEquals(listOf("prepare", "publish", "retire"), session.log)
        assertEquals(ShizukuLifecycle.State.OFF, lifecycle.state.value)
    }

    /** What a real retirement deadline or a replaced epoch looks like coming out of the session. */
    private class RetirementDeadline : CancellationException("retirement deadline")

    /** The states the row renders, which have to agree on exactly one meaning of "nothing is happening". */
    @Test
    fun onlyOffIsNeitherOnNorBusy() {
        for (state in ShizukuLifecycle.State.entries) {
            assertEquals("$state", state == ShizukuLifecycle.State.OFF, !state.on && !state.busy)
        }
    }

    /**
     * Everything attached to a failure, including to the copies coroutines make of it.
     *
     * `withContext` recovers stack traces by rethrowing a copy of the exception with the original as its
     * cause, so a caller receives the attachment one level down rather than on the object it caught. What
     * production reports is the whole chain, so this asserts against the whole chain.
     */
    private fun Throwable.attachments(): List<Throwable> =
        suppressed.toList() + (cause?.takeUnless { it === this }?.attachments().orEmpty())

    private fun runCatchingFailure(block: suspend () -> Unit): Throwable? = runBlocking {
        runCatchingSuspend(block)
    }

    private suspend fun runCatchingSuspend(block: suspend () -> Unit): Throwable? = try {
        block()
        null
    } catch (e: Throwable) {
        e
    }
}
