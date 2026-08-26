package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.updateAndGet

/**
 * The one coherent answer to "which binder, of which generation, and has it arrived yet".
 *
 * Shizuku delivers arrival and death on the main handler while every privileged operation runs on its own
 * lane, so these facts are read and written by different threads. Publishing them as separate fields would
 * let a reader combine a binder from one delivery with a generation from another - and that combination is
 * precisely the thing an epoch exists to make impossible, because it reads as "still current" across the
 * replacement it was supposed to catch. So they move together, as one immutable snapshot swapped
 * atomically, and a caller either holds a whole publication or holds none.
 *
 * Deliberately free of Android and Shizuku types: what it owns is a publication race, which is testable on
 * its own and is the part worth testing.
 */
internal class BinderPublisher<B : Any> {
    /** One indivisible answer, with identity equality so even redelivery of the same binder is a successor. */
    class Publication<B : Any> internal constructor(
        val generation: Long,
        val binder: B?,
    ) {
        override fun toString() = "publication $generation(${binder ?: "none"})"
    }

    private val state = MutableStateFlow(Publication<B>(0, null))

    val current: Publication<B> get() = state.value

    /** Publishes a live successor and wakes waiters through [state]. */
    fun received(binder: B): Publication<B> = replace(binder)

    /** Publishes a dead successor; waiters re-read it and keep waiting for a live publication. */
    fun died(): Publication<B> = replace(null)

    private fun replace(binder: B?) = state.updateAndGet { previous ->
        Publication(previous.generation + 1, binder)
    }

    /**
     * The whole publication that carries a binder, never a binder on its own: an authorization that took the
     * binder here and the generation from a later read would pin an identity nothing can invalidate.
     */
    suspend fun awaitBinder(): Publication<B> = state.first { it.binder != null }

    /** Whether [publication] is still the current one, which is the only meaning "current" has. */
    fun holds(publication: Publication<B>) = state.value === publication
}
