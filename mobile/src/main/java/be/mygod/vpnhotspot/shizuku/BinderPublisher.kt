package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.updateAndGet
import kotlinx.coroutines.launch
import java.util.concurrent.atomic.AtomicInteger

internal class BinderPublisher<B : Any> {
    class Publication<B : Any> internal constructor(val binder: B?)

    // Fresh identity makes even redelivery of the same binder supersede prior work.
    private val state = MutableStateFlow(Publication<B>(null))

    val current: Publication<B> get() = state.value

    fun received(binder: B): Publication<B> = replace(binder)

    fun died(): Publication<B> = replace(null)

    private fun replace(binder: B?) = state.updateAndGet { Publication(binder) }

    suspend fun awaitBinder(): Publication<B> = state.first { it.binder != null }

    fun holds(publication: Publication<B>) = state.value === publication

    suspend fun awaitSuperseded(publication: Publication<B>) = state.first { it !== publication }

    inner class Attempt(private val publication: Publication<B>) {
        val token = nextToken.getAndIncrement()
        private val answer = CompletableDeferred<Int?>()

        fun deliver(token: Int, result: Int) {
            if (token == this.token && holds(publication)) answer.complete(result)
        }

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
        // Permission results share Shizuku's process-wide listener list.
        private val nextToken = AtomicInteger()
    }
}
