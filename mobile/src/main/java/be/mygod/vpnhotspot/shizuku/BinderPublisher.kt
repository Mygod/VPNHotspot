package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.updateAndGet
import kotlinx.coroutines.launch
import java.util.concurrent.atomic.AtomicInteger

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

    /**
     * Suspends until [publication] is no longer the current one, and answers with whatever replaced it.
     *
     * The awaitable half of [holds], and it exists for the one wait nothing else can end: an attempt pinned
     * to a publication has nothing left to wait for exactly when that publication is gone, because an answer
     * arriving against it could no longer be validated - not because nothing could deliver it, which is
     * [Attempt]'s business. Identity again rather than binder equality, so a redelivery of the same binder
     * wakes this too - the successor is a generation nothing has authorized anything against, and treating
     * it as the same one would leave the wait pointed at a publication no check would accept any more.
     *
     * Answered from the state rather than from an edge, so a publication that was already superseded before
     * anyone waited on it returns at once: every publication is a new object, which is what makes the state
     * itself sufficient and leaves no update to miss.
     */
    suspend fun awaitSuperseded(publication: Publication<B>) = state.first { it !== publication }

    /**
     * One attempt at an answer owed against one publication, and the only thing that may complete it.
     *
     * It exists because the answers are *not* publication-scoped, however much a publication is the only
     * thing a caller can validate. Shizuku is the case in point: it hands every service the same
     * process-global application binder and dispatches every permission result to one process-global
     * listener list, so a replaced-but-live service can still answer a request an attempt has already given
     * up on - and answer it after a successor has asked its own question. [token] is what keeps the two
     * apart. Every attempt takes a fresh one and accepts no other, so a late answer to an abandoned attempt
     * completes nothing.
     *
     * [publication] being superseded is the attempt's other ending, and it is terminal for the *attempt*
     * rather than for the answer: what has gone is the thing the attempt was pinned to, and an answer
     * against it could no longer be validated whatever it said. So [deliver] takes an answer only while that
     * publication is still current, and once it is not, the one thing the slot can still take is the
     * supersession. That is an ordering and not a race: publishing a successor and delivering an answer
     * arrive on the same single thread - Shizuku's handler, in production - so one of them strictly precedes
     * the other, and which of the two [await] reports does not depend on when a caller gets around to
     * calling it.
     */
    inner class Attempt(private val publication: Publication<B>) {
        /** Distinct for every attempt in this process, which is the whole of what a correlation key needs. */
        val token = nextToken.getAndIncrement()
        private val answer = CompletableDeferred<Int?>()

        /**
         * Takes one answer, and only this attempt's own, and only while [publication] is still current. A
         * late one is refused right here rather than left for [await] to sort out, which it could no longer
         * do: a full slot is what [await] reads, so an answer already in it would be reported however stale
         * the publication had become before it arrived.
         */
        fun deliver(token: Int, result: Int) {
            if (token == this.token && holds(publication)) answer.complete(result)
        }

        /**
         * The result delivered while [publication] was still current, or null once it has been superseded
         * without one. Suspends for as long as that takes: nothing here bounds the wait, and a cancellation
         * stays the caller's own.
         */
        suspend fun await(): Int? = coroutineScope {
            val supersession = launch {
                awaitSuperseded(publication)
                answer.complete(null)
            }
            try {
                answer.await()
            } finally {
                supersession.cancel()
            }
        }
    }

    companion object {
        /**
         * Process-wide rather than per publisher, because what a token has to be distinct from is every
         * other attempt whose answer could reach the same process-global listener list.
         */
        private val nextToken = AtomicInteger()
    }
}
