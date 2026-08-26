package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.async
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.yield
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotSame
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The publication race on its own, which is the part of [ShizukuEpoch] worth testing without a device: what
 * an epoch is pinned to has to be one indivisible answer, and a waiter has to be woken by whatever superseded
 * what it was waiting for. An attempt adds the other half of that - which answers it may take at all - and
 * it matters here rather than only in [ShizukuEpoch] because Shizuku's permission results reach one
 * process-global listener list instead of the publication that was asked.
 */
class BinderPublisherTest {
    private class FakeBinder(private val name: String) {
        override fun toString() = name
    }

    private val publisher = BinderPublisher<FakeBinder>()

    @Test
    fun startsWithNothingPublished() {
        assertEquals(0, publisher.current.generation)
        assertNull(publisher.current.binder)
    }

    @Test
    fun eachDeliveryIsANewGeneration() {
        val one = publisher.received(FakeBinder("one"))
        val two = publisher.received(FakeBinder("two"))
        assertEquals(1, one.generation)
        assertEquals(2, two.generation)
        assertNotSame(one.binder, two.binder)
    }

    /** "Current" means this exact publication, not a binder that happens to match. */
    @Test
    fun onlyTheLatestPublicationIsHeld() {
        val one = publisher.received(FakeBinder("one"))
        assertTrue(publisher.holds(one))
        val two = publisher.received(FakeBinder("two"))
        assertTrue(publisher.holds(two))
        assertTrue(!publisher.holds(one))
    }

    /** A death is a generation of its own, so an epoch pinned before it can never read as current. */
    @Test
    fun deathSupersedesTheLivePublication() {
        val alive = publisher.received(FakeBinder("one"))
        val dead = publisher.died()
        assertTrue(!publisher.holds(alive))
        assertTrue(publisher.holds(dead))
        assertNull(dead.binder)
        assertEquals(2, dead.generation)
    }

    /**
     * The same binder delivered twice is still two generations. Anything pinned to the first is stale, which
     * is the conservative answer: Shizuku's own listeners are what report a replacement, and an epoch that
     * survived one would be trusting an identity nothing re-checked.
     */
    @Test
    fun redeliveryOfTheSameBinderStillSupersedes() {
        val binder = FakeBinder("one")
        val first = publisher.received(binder)
        val second = publisher.received(binder)
        assertNotSame(first, second)
        assertSame(binder, first.binder)
        assertSame(binder, second.binder)
        assertEquals(first.generation + 1, second.generation)
        assertFalse(publisher.holds(first))
        assertTrue(publisher.holds(second))
    }

    @Test
    fun anAlreadyDeliveredBinderIsAwaitedWithoutParking() = runBlocking {
        val published = publisher.received(FakeBinder("one"))
        assertSame(published, publisher.awaitBinder())
    }

    /** A waiter parked before any delivery is woken by the delivery and gets that whole publication. */
    @Test
    fun awaitingWakesOnDelivery() = runBlocking {
        var seen: BinderPublisher.Publication<FakeBinder>? = null
        val waiter = launch { seen = publisher.awaitBinder() }
        yield()
        assertNull(seen)
        val published = publisher.received(FakeBinder("one"))
        waiter.join()
        assertSame(published, seen)
    }

    /** A death while somebody is waiting leaves them pending until a later live publication arrives. */
    @Test
    fun awaitingSurvivesADeathAndResumesOnTheNextDelivery() = runBlocking {
        var seen: BinderPublisher.Publication<FakeBinder>? = null
        val waiter = launch { seen = publisher.awaitBinder() }
        yield()
        publisher.died()
        yield()
        assertNull(seen)
        val published = publisher.received(FakeBinder("two"))
        waiter.join()
        assertSame(published, seen)
        assertEquals(2, published.generation)
    }

    /**
     * Delivery immediately followed by death: a waiter must not resume holding the binder that has just been
     * superseded, because that is precisely the combination an epoch check exists to reject.
     */
    @Test
    fun immediateDeliveryAndDeathStayPendingForTheNextDelivery() = runBlocking {
        var seen: BinderPublisher.Publication<FakeBinder>? = null
        val waiter = launch(start = CoroutineStart.UNDISPATCHED) { seen = publisher.awaitBinder() }
        val superseded = publisher.received(FakeBinder("one"))
        publisher.died()
        yield()
        // The dead successor wins before the waiter can return the briefly live publication.
        assertNull(seen)
        assertFalse(publisher.holds(superseded))
        val published = publisher.received(FakeBinder("two"))
        waiter.join()
        assertSame(published, seen)
        assertTrue(publisher.holds(seen!!))
    }

    /**
     * The other half of what an epoch needs: a permission result, or any other answer owed against one
     * publication, becomes *unacceptable* exactly when that publication stops being the current one. Not
     * undeliverable - whether anything could still deliver it is a separate question, and for Shizuku's
     * permission results the answer is yes. Until then the wait is the human's to end and nothing here may
     * shorten it.
     */
    @Test
    fun supersessionStaysPendingWhileThePublicationIsCurrent() = runBlocking {
        val published = publisher.received(FakeBinder("one"))
        var seen: BinderPublisher.Publication<FakeBinder>? = null
        val waiter = launch { seen = publisher.awaitSuperseded(published) }
        yield()
        assertNull(seen)
        assertTrue(publisher.holds(published))
        waiter.cancel()
    }

    /** Death invalidates the publication, so an answer owed against it could no longer be accepted. */
    @Test
    fun deathSupersedesAWaiter() = runBlocking {
        val published = publisher.received(FakeBinder("one"))
        var seen: BinderPublisher.Publication<FakeBinder>? = null
        val waiter = launch { seen = publisher.awaitSuperseded(published) }
        yield()
        assertNull(seen)
        val dead = publisher.died()
        waiter.join()
        assertSame(dead, seen)
        assertNull(seen!!.binder)
    }

    /**
     * A live replacement supersedes it too, and this is the case that makes the reason explicit: the
     * replaced-but-live service can still answer, through the process-global listener list Shizuku dispatches
     * every permission result to. What ends the wait is that nothing about the departed publication may be
     * believed any more, never that delivery became impossible.
     */
    @Test
    fun replacementSupersedesAWaiter() = runBlocking {
        val published = publisher.received(FakeBinder("one"))
        var seen: BinderPublisher.Publication<FakeBinder>? = null
        val waiter = launch { seen = publisher.awaitSuperseded(published) }
        yield()
        assertNull(seen)
        val replacement = publisher.received(FakeBinder("two"))
        waiter.join()
        assertSame(replacement, seen)
    }

    /**
     * Redelivery of the same binder wakes the waiter, exactly as it fails
     * [redeliveryOfTheSameBinderStillSupersedes]. The two questions have to answer alike: a waiter left
     * pending on a publication [BinderPublisher.holds] already rejects would be waiting for an answer no
     * check would accept if it came.
     */
    @Test
    fun redeliveryOfTheSameBinderSupersedesAWaiter() = runBlocking {
        val binder = FakeBinder("one")
        val published = publisher.received(binder)
        var seen: BinderPublisher.Publication<FakeBinder>? = null
        val waiter = launch { seen = publisher.awaitSuperseded(published) }
        yield()
        assertNull(seen)
        val again = publisher.received(binder)
        waiter.join()
        assertNotSame(published, seen)
        assertSame(again, seen)
        assertSame(binder, seen!!.binder)
    }

    /**
     * The wake-up that must not be lost: whatever superseded the publication may already have happened
     * before anything waited on it - Shizuku can die between the request being issued and the wait starting -
     * so the answer has to come from the state and not from an edge.
     */
    @Test
    fun anAlreadySupersededPublicationIsAnsweredWithoutParking() = runBlocking {
        val published = publisher.received(FakeBinder("one"))
        val dead = publisher.died()
        assertSame(dead, publisher.awaitSuperseded(published))
    }

    /**
     * Two supersessions inside one turn: the waiter is answered with what is current when it resumes rather
     * than with the one that raced past, which is the same conflation
     * [immediateDeliveryAndDeathStayPendingForTheNextDelivery] relies on and is what makes the answer
     * usable in a report.
     */
    @Test
    fun supersessionAnswersWithTheLatestPublication() = runBlocking {
        val published = publisher.received(FakeBinder("one"))
        var seen: BinderPublisher.Publication<FakeBinder>? = null
        val waiter = launch(start = CoroutineStart.UNDISPATCHED) {
            seen = publisher.awaitSuperseded(published)
        }
        assertNull(seen)
        publisher.died()
        val latest = publisher.received(FakeBinder("two"))
        waiter.join()
        assertSame(latest, seen)
        assertTrue(publisher.holds(seen!!))
    }

    /**
     * A token is only ever a correlation key, so distinctness is the whole of what it promises - and it has
     * to hold across publications too, because two attempts pinned to the same publication are still two
     * questions whose answers reach the same process-global listener list.
     */
    @Test
    fun eachAttemptTakesADistinctToken() {
        val published = publisher.received(FakeBinder("one"))
        val tokens = setOf(publisher.Attempt(published).token, publisher.Attempt(published).token,
            publisher.Attempt(publisher.received(FakeBinder("two"))).token)
        assertEquals(3, tokens.size)
    }

    /**
     * The misattribution a shared request code allowed: an answer to a question an earlier attempt asked
     * arrives while a later one is waiting, and completes it. The result values are opaque here - only the
     * token decides - so this delivers the *wrong* one to make an accepted answer visible.
     */
    @Test
    fun anAttemptTakesNoOtherAttemptsAnswer() = runBlocking {
        val published = publisher.received(FakeBinder("one"))
        val abandoned = publisher.Attempt(published)
        val attempt = publisher.Attempt(published)
        val waiter = async { attempt.await() }
        yield()
        attempt.deliver(abandoned.token, -1)
        yield()
        assertTrue(waiter.isActive)
        attempt.deliver(attempt.token, 0)
        assertEquals(0, waiter.await())
    }

    /** One answer and no more: a second delivery cannot revise what the human already said. */
    @Test
    fun anAnsweredAttemptKeepsItsFirstAnswer() = runBlocking {
        val attempt = publisher.Attempt(publisher.received(FakeBinder("one")))
        attempt.deliver(attempt.token, 0)
        attempt.deliver(attempt.token, -1)
        assertEquals(0, attempt.await())
    }

    /**
     * A live replacement is the interesting supersession, because it is exactly the case where something
     * could still deliver: the successor's service can answer through the same process-global listener list.
     * The attempt ends anyway - what it was pinned to has gone, so no answer about it could be validated.
     */
    @Test
    fun aReplacementEndsAnAttemptWithoutAnAnswer() = runBlocking {
        val published = publisher.received(FakeBinder("one"))
        val attempt = publisher.Attempt(published)
        val waiter = async { attempt.await() }
        yield()
        assertTrue(waiter.isActive)
        publisher.received(FakeBinder("two"))
        assertNull(waiter.await())
    }

    /**
     * The stale-before-dispatch case as the attempt sees it: whatever an owner validated before issuing its
     * request, the publication can be gone by the time the wait starts, and then the attempt is over before
     * it parks rather than waiting for an answer it would refuse.
     */
    @Test
    fun anAttemptOnASupersededPublicationIsAlreadyOver() = runBlocking {
        val published = publisher.received(FakeBinder("one"))
        val attempt = publisher.Attempt(published)
        publisher.died()
        assertNull(attempt.await())
    }

    /**
     * The pair of the case below: an answer that arrived while the publication was still current was taken
     * then, so a supersession afterwards does not undo it.
     */
    @Test
    fun anAnswerAlreadyGivenSurvivesASupersession() = runBlocking {
        val published = publisher.received(FakeBinder("one"))
        val attempt = publisher.Attempt(published)
        attempt.deliver(attempt.token, 0)
        publisher.died()
        assertEquals(0, attempt.await())
    }

    /**
     * The ordering an already-full slot would otherwise decide on its own: the publication goes, its answer
     * arrives anyway, and only then does anybody wait. Supersession came first, so that is what the attempt
     * reports - it is not a question of which of the two a caller happens to be in time to see, because
     * [BinderPublisher.Attempt.deliver] refused the answer when it arrived rather than leaving [await] to
     * find it already recorded.
     */
    @Test
    fun anAnswerDeliveredAfterASupersessionIsRefusedBeforeAnyWait() = runBlocking {
        val published = publisher.received(FakeBinder("one"))
        val attempt = publisher.Attempt(published)
        publisher.received(FakeBinder("two"))
        attempt.deliver(attempt.token, 0)
        assertNull(attempt.await())
    }

    /**
     * And the other way round, which is what makes an abandoned attempt harmless: the answer it was waiting
     * for arrives after it has already ended, and changes nothing it reports.
     */
    @Test
    fun anEndedAttemptIsNotRevivedByItsOwnLateAnswer() = runBlocking {
        val published = publisher.received(FakeBinder("one"))
        val attempt = publisher.Attempt(published)
        publisher.received(FakeBinder("two"))
        assertNull(attempt.await())
        attempt.deliver(attempt.token, 0)
        assertNull(attempt.await())
    }

    /**
     * Cancellation stays cancellation and is written nowhere: the owner's stop ends the wait without
     * recording either an answer or a supersession, which are the only two things the attempt may report.
     * A caller that cancelled and asked again is therefore still waiting rather than being told Shizuku has
     * gone away.
     */
    @Test
    fun cancellingAWaitRecordsNothing() = runBlocking {
        val attempt = publisher.Attempt(publisher.received(FakeBinder("one")))
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
