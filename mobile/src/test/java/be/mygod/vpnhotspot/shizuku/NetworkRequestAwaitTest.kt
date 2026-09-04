package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Rule
import org.junit.Test
import org.junit.rules.Timeout

class NetworkRequestAwaitTest {
    @get:Rule
    val bound: Timeout = Timeout.seconds(20)

    @Test
    fun aPublishedResultIsWhatTheBarrierAnswers() = runBlocking {
        assertEquals("published", awaitNetworkRequest(CompletableDeferred("published"), CompletableDeferred()))
    }

    @Test
    fun anExpiredRequestEndsAnIncompleteBarrier() = runBlocking {
        val result = CompletableDeferred<Unit>()
        try {
            awaitNetworkRequest(result, CompletableDeferred(Unit))
            fail("an expired request must fail the publication")
        } catch (_: NetworkRequestExpiredException) { }
        assertFalse("the positive barrier was never owed", result.isCompleted)
    }

    @Test
    fun anExpiredRequestOutranksAResultQueuedBesideIt() = runBlocking {
        try {
            awaitNetworkRequest(CompletableDeferred("late publication"), CompletableDeferred(Unit))
            fail("an expired request must not commit a result it can no longer own")
        } catch (_: NetworkRequestExpiredException) { }
    }

    @Test
    fun aBarrierWithNoAnswerEndsOnCancellationAlone() = runBlocking {
        val entered = CompletableDeferred<Unit>()
        val ending = CompletableDeferred<Throwable>()
        val waiter = launch {
            entered.complete(Unit)
            try {
                awaitNetworkRequest(CompletableDeferred<Unit>(), CompletableDeferred())
            } catch (e: Throwable) {
                ending.complete(e)
                throw e
            }
        }
        entered.await()
        waiter.cancelAndJoin()
        assertTrue("the wait ended on ${ending.await()}", ending.await() is CancellationException)
    }
}
