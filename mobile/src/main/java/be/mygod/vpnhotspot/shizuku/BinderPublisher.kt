package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.CompletableDeferred
import java.util.concurrent.atomic.AtomicReference

/**
 * The one coherent answer to "which binder, of which generation, and has it arrived yet".
 *
 * Shizuku delivers arrival and death on the main handler while every privileged operation runs on its own
 * lane, so these facts are read and written by different threads. Publishing them as separate fields would
 * let a reader combine a binder from one delivery with a generation from another - and that combination is
 * precisely the thing an epoch exists to make impossible, because it reads as "still current" across the
 * replacement it was supposed to catch. So the three move together, as one immutable snapshot swapped
 * atomically, and a caller either holds a whole publication or holds none.
 *
 * Deliberately free of Android and Shizuku types: what it owns is a publication race, which is testable on
 * its own and is the part worth testing.
 */
internal class BinderPublisher<B : Any> {
    /**
     * One indivisible answer. [arrived] belongs to the publication rather than to the publisher, so a waiter
     * that parked on this generation is woken by whatever superseded it and re-reads rather than resuming
     * against a binder that has since been replaced.
     */
    class Publication<B : Any> internal constructor(
        val generation: Long,
        val binder: B?,
        internal val arrived: CompletableDeferred<Unit>,
    ) {
        override fun toString() = "publication $generation(${binder ?: "none"})"
    }

    private val state = AtomicReference(Publication<B>(0, null, CompletableDeferred()))

    val current: Publication<B> get() = state.get()

    /**
     * Publishes a successor and wakes everyone parked on its predecessor. The successor is born already
     * arrived, so a caller reaching it afterwards does not wait for an event that has happened.
     */
    fun received(binder: B): Publication<B> = replace(binder, CompletableDeferred<Unit>().apply {
        complete(Unit)
    })

    /**
     * The same, for a death: the successor carries no binder and has not arrived. Predecessor waiters are
     * still woken, because waking and re-reading is how a waiter learns it has to keep waiting.
     */
    fun died(): Publication<B> = replace(null, CompletableDeferred())

    private fun replace(binder: B?, arrived: CompletableDeferred<Unit>): Publication<B> {
        while (true) {
            val previous = state.get()
            val next = Publication(previous.generation + 1, binder, arrived)
            if (state.compareAndSet(previous, next)) {
                previous.arrived.complete(Unit)
                return next
            }
        }
    }

    /**
     * The whole publication that carries a binder, never a binder on its own: an authorization that took the
     * binder here and the generation from a later read would pin an identity nothing can invalidate.
     */
    suspend fun awaitBinder(): Publication<B> {
        while (true) {
            val publication = state.get()
            if (publication.binder != null) return publication
            publication.arrived.await()
        }
    }

    /** Whether [publication] is still the current one, which is the only meaning "current" has. */
    fun holds(publication: Publication<B>) = state.get() === publication
}
