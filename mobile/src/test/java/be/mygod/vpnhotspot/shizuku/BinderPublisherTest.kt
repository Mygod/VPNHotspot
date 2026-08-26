package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.CoroutineStart
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
 * what it was waiting for.
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
}
