package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.async
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.yield
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotSame
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class BinderPublisherTest {
    private class FakeBinder

    private val publisher = BinderPublisher<FakeBinder>()

    @Test
    fun publicationProgressionUsesIdentity() {
        assertNull(publisher.current.binder)
        val binder = FakeBinder()
        val first = publisher.received(binder)
        val redelivered = publisher.received(binder)
        assertNotSame(first, redelivered)
        assertSame(binder, redelivered.binder)
        assertTrue(!publisher.holds(first))
        assertTrue(publisher.holds(redelivered))
        val dead = publisher.died()
        assertTrue(!publisher.holds(redelivered))
        assertTrue(publisher.holds(dead))
        assertNull(dead.binder)
    }

    @Test
    fun awaitBinderIgnoresDeathAndReturnsTheNextLivePublication() = runBlocking {
        var seen: BinderPublisher.Publication<FakeBinder>? = null
        val waiter = launch { seen = publisher.awaitBinder() }
        yield()
        publisher.died()
        yield()
        assertNull(seen)
        val live = publisher.received(FakeBinder())
        waiter.join()
        assertSame(live, seen)
        assertSame(live, publisher.awaitBinder())
    }

    @Test
    fun awaitSupersededUsesCurrentStateWithoutMissingAReplacement() = runBlocking {
        val binder = FakeBinder()
        val first = publisher.received(binder)
        val second = publisher.received(binder)
        assertSame(second, publisher.awaitSuperseded(first))

        val waiter = async { publisher.awaitSuperseded(second) }
        yield()
        assertTrue(waiter.isActive)
        val dead = publisher.died()
        assertSame(dead, waiter.await())
    }

    @Test
    fun supersessionReturnsTheLatestPublication() = runBlocking {
        val first = publisher.received(FakeBinder())
        val waiter = async(start = CoroutineStart.UNDISPATCHED) { publisher.awaitSuperseded(first) }
        publisher.died()
        val latest = publisher.received(FakeBinder())
        assertSame(latest, waiter.await())
    }

    @Test
    fun attemptTokensPreventAnswerMisattribution() = runBlocking {
        val published = publisher.received(FakeBinder())
        val abandoned = publisher.Attempt(published)
        val attempt = publisher.Attempt(published)
        assertTrue(abandoned.token != attempt.token)
        val waiter = async { attempt.await() }
        attempt.deliver(abandoned.token, -1)
        yield()
        assertTrue(waiter.isActive)
        attempt.deliver(attempt.token, 0)
        assertEquals(0, waiter.await())
    }

    @Test
    fun attemptAcceptsAnswersOnlyWhileItsPublicationIsCurrent() = runBlocking {
        val first = publisher.received(FakeBinder())
        val answered = publisher.Attempt(first)
        answered.deliver(answered.token, 0)
        publisher.died()
        assertEquals(0, answered.await())

        val second = publisher.received(FakeBinder())
        val stale = publisher.Attempt(second)
        publisher.received(FakeBinder())
        stale.deliver(stale.token, 0)
        assertNull(stale.await())
    }

    @Test
    fun cancellingAnAttemptWaitRecordsNothing() = runBlocking {
        val attempt = publisher.Attempt(publisher.received(FakeBinder()))
        val cancelled = async { attempt.await() }
        yield()
        cancelled.cancelAndJoin()
        val resumed = async { attempt.await() }
        yield()
        assertTrue(resumed.isActive)
        attempt.deliver(attempt.token, 0)
        assertEquals(0, resumed.await())
    }
}
