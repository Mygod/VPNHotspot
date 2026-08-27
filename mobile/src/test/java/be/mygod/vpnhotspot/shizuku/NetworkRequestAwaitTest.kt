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

/**
 * The one rule that decides whether a publication barrier can end at all.
 *
 * A native network ConnectivityService could not create produces no agent callback, so every positive barrier
 * a startup waits on would wait forever; the timed exact request's `onUnavailable` is the platform's only
 * negative answer, and this is where it beats the barrier it is raced against. Driven through the free
 * function rather than through [RequestCallback], because that callback extends a framework class a JVM test
 * cannot construct - the two `CompletableDeferred` barriers are the whole of what the rule reads.
 */
class NetworkRequestAwaitTest {
    /**
     * A failure bound and never a synchronization device: a rule that stopped racing the negative terminal
     * would hang instead of failing, and this only decides how long that regression may hang CI first.
     */
    @get:Rule
    val bound: Timeout = Timeout.seconds(20)

    @Test
    fun aPublishedResultIsWhatTheBarrierAnswers() = runBlocking {
        assertEquals("published", awaitNetworkRequest(CompletableDeferred("published"), CompletableDeferred()))
    }

    /** The whole point: the barrier the platform will never complete ends anyway, and ends as a failure. */
    @Test
    fun anExpiredRequestEndsAnIncompleteBarrier() = runBlocking {
        val result = CompletableDeferred<Unit>()
        try {
            awaitNetworkRequest(result, CompletableDeferred(Unit))
            fail("an expired request must fail the publication")
        } catch (_: NetworkRequestExpiredException) { }
        // Which is what makes this the interesting case rather than an ordering detail: `onNetworkCreated`
        // never arrived, so nothing but the expiry could have ended this.
        assertFalse("the positive barrier was never owed", result.isCompleted)
    }

    /**
     * Expiry outranks a result delivered in the same turn, because the request that result belongs to has
     * already been removed by ConnectivityService and dropped by `ConnectivityManager`.
     */
    @Test
    fun anExpiredRequestOutranksAResultQueuedBesideIt() = runBlocking {
        try {
            awaitNetworkRequest(CompletableDeferred("late publication"), CompletableDeferred(Unit))
            fail("an expired request must not commit a result it can no longer own")
        } catch (_: NetworkRequestExpiredException) { }
    }

    /** No platform answer means no ending of its own: a stop is still the only terminal such a wait has. */
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
