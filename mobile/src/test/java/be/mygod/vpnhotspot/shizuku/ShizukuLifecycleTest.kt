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
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.Timeout
import java.io.IOException

class ShizukuLifecycleTest {
    @get:Rule
    val bound: Timeout = Timeout.seconds(20)

    private class Recorder(
        private val publishFails: Throwable? = null,
        var withdrawal: Withdrawal = Withdrawal.Fences,
        var settleFails: Throwable? = null,
        var prepareFails: Throwable? = null,
        private val reportFails: Throwable? = null,
    ) : ShizukuLifecycle.Session {
        sealed interface Withdrawal {
            data object Fences : Withdrawal
            class LeavesLocalDebt(val cause: Throwable) : Withdrawal
            class LeavesResidual(val cause: Throwable) : Withdrawal
        }
        var debt = false

        private val entries = MutableStateFlow(emptyList<String>())
        val log get() = entries.value
        val reported = mutableListOf<Exception>()

        private val uncaught = mutableListOf<Throwable>()
        val handler = CoroutineExceptionHandler { _, e -> uncaught += e }
        fun takeUncaught() = uncaught.map { it.cause ?: it }.also { uncaught.clear() }
        fun assertNothingUncaught() = assertEquals("a lifespan failed in a way no test arranged",
            emptyList<Throwable>(), uncaught)

        val intents = mutableListOf<Boolean>()

        var fenced: Boolean? = null
            private set
        var settlements = 0
            private set

        var settleGate: CompletableDeferred<Unit>? = null
        var prepareGate: CompletableDeferred<Unit>? = null
        var publishGate: CompletableDeferred<Unit>? = null
        var retireGate: CompletableDeferred<Unit>? = null
        var fenceQueries = 0
            private set

        var terminal = CompletableDeferred<Unit>()

        private fun record(entry: String) {
            entries.value = entries.value + entry
        }

        suspend fun awaitLog(vararg expected: String) {
            val prefix = expected.toList()
            entries.first { it.size >= prefix.size && it.subList(0, prefix.size) == prefix }
        }

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

        override suspend fun retire(owner: Job) {
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

        override suspend fun fenced(): Boolean {
            fenceQueries++
            return !debt
        }

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

    private fun driving(
        session: Recorder = Recorder(),
        block: suspend CoroutineScope.(ShizukuLifecycle, Recorder) -> Unit,
    ) = runBlocking {
        val owner = CoroutineScope(coroutineContext + Dispatchers.Unconfined + SupervisorJob() +
                session.handler)
        val lifecycle = ShizukuLifecycle(owner, session)
        try {
            block(lifecycle, session)
        } finally {
            session.settleGate?.complete(Unit)
            session.prepareGate?.complete(Unit)
            session.retireGate?.complete(Unit)
            session.terminal.complete(Unit)
            owner.coroutineContext.job.cancelAndJoin()
        }
        session.assertNothingUncaught()
        assertTrue("a lifespan outlived the scope that owned it", lifecycle.idle)
    }

    @Test
    fun aStopPublishesOffImmediatelyAndCancelsStartup() = driving { lifecycle, session ->
        session.prepareGate = CompletableDeferred()
        lifecycle.start()
        session.awaitLog("on", "prepare")

        lifecycle.stop()
        assertEquals("off the moment it is asked for", listOf(true, false), session.intents)

        session.prepareGate!!.complete(Unit)
        session.prepareGate = null
        session.awaitLog("on", "prepare", "off", "settled")
        assertTrue(lifecycle.idle)
        assertEquals(1, session.settlements)
    }

    @Test
    fun anUnsupportedPlatformGateCreatesNoCleanupDebt() = driving(Recorder(
        prepareFails = UnsupportedDeviceException("missing tethering capability", expected = true))) {
            lifecycle, session ->
        lifecycle.start()
        session.awaitLog("on", "prepare", "off", "settled")

        assertEquals(0, session.log.count { it == "publish" })
        assertEquals(0, session.log.count { it == "retire" })
        assertEquals(true, session.fenced)
        assertTrue(session.reported.single() is UnsupportedDeviceException)
        assertTrue(lifecycle.idle)
    }

    @Test
    fun bandwidthCapabilityAvailabilityIncludesUExtension16() {
        assertFalse(ShizukuTestNetwork.expectsBandwidthCapability(33, 99))
        assertFalse(ShizukuTestNetwork.expectsBandwidthCapability(34, 15))
        assertTrue(ShizukuTestNetwork.expectsBandwidthCapability(34, 16))
        assertTrue(ShizukuTestNetwork.expectsBandwidthCapability(35, 16))
        assertTrue(ShizukuTestNetwork.expectsBandwidthCapability(36, 0))
    }

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
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "on", "off", "settled")
        assertTrue(lifecycle.idle)
    }

    @Test
    fun aRecreatedInstanceWaitsForTheDestroyedOnesFinalizer() = driving { destroyed, session ->
        destroyed.start()
        session.awaitLog("on", "prepare", "publish", "run")

        session.retireGate = CompletableDeferred()
        destroyed.destroy()
        session.awaitLog("on", "prepare", "publish", "run", "off", "retire")

        val recreatedScope = CoroutineScope(coroutineContext + Dispatchers.Unconfined + SupervisorJob() +
                session.handler)
        try {
            val recreated = ShizukuLifecycle(recreatedScope, session)
            recreated.start()
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
            session.retireGate?.complete(Unit)
            recreatedScope.coroutineContext.job.cancelAndJoin()
        }
    }

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

    @Test
    fun aFailedReportCannotSkipCleanupOrBookkeeping() {
        val reporterDied = IllegalStateException("the reporter is gone")
        driving(Recorder(publishFails = IOException("agent refused"), reportFails = reporterDied)) {
                lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "off", "retire", "settled")
            assertEquals(1, session.reported.size)
            assertEquals(1, session.log.count { it == "retire" })
            assertEquals(1, session.fenceQueries)
            assertEquals(true, session.fenced)
            assertEquals(1, session.settlements)
            assertTrue(lifecycle.idle)
            assertSame(reporterDied, session.takeUncaught().single())
        }
    }

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

            lifecycle.stop()
            assertEquals("a stop with nothing in flight is a no-op", listOf(true, false), session.intents)
            assertEquals("and settles nothing a second time", 1, session.settlements)
            assertTrue(lifecycle.idle)
            assertFalse("so the component is kept over the debt", session.fenced())
            assertEquals("by one question, not a loop", 2, session.fenceQueries)

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

    @Test
    fun destructionOwnsTheLastAttemptAtWhatItCancelledAndCouldNotFence() =
        driving(Recorder(withdrawal =
            Recorder.Withdrawal.LeavesLocalDebt(IllegalStateException("child survived")))) {
                lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run")

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

    @Test
    fun predecessorDebtIsNotRetriedTwiceByOneStart() =
        driving(Recorder(withdrawal =
            Recorder.Withdrawal.LeavesLocalDebt(IllegalStateException("child survived")))) {
                lifecycle, session ->
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run")
            lifecycle.stop()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced")

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

            session.settleFails = null
            lifecycle.start()
            session.awaitLog("on", "prepare", "publish", "run", "off", "retire", "unfenced",
                "on", "settle", "off", "unfenced",
                "on", "settle", "prepare", "publish", "run")
            assertEquals("two starts, two attempts, never two in one", 2,
                session.log.count { it == "settle" })
            assertEquals(1, session.log.count { it == "retire" })
        }

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

    @Test
    fun aStaleOwnerCannotWithdrawItsSuccessorsIntent() {
        val restore = ShizukuTestNetwork.intent.value
        val predecessor = Job()
        val successor = Job()
        try {
            ShizukuTestNetwork.publishIntent(predecessor)
            assertSame(predecessor, ShizukuTestNetwork.intent.value)

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
