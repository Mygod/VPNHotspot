package be.mygod.vpnhotspot.util

import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.ProducerScope
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.channels.onFailure
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.channelFlow
import kotlinx.coroutines.withContext

/**
 * Scope handed to a [binderCallbackFlow] registration block for forwarding callback invocations
 * into the flow.
 */
class BinderCallbackScope<T>(private val name: String, private val scope: ProducerScope<T>) {
    fun push(event: T) = scope.trySend(event).onFailure {
        scope.close(it ?: IllegalStateException("Flow buffer rejected $name"))
    }.isSuccess

    fun finish(event: T) {
        if (push(event)) scope.close()
    }
}

/**
 * Bridge a callback registered with a Binder-backed framework service (WifiManager, TetheringManager,
 * …) into a cold [Flow].
 *
 * [register] is invoked once per collector: it should register the callback (forwarding invocations
 * through [BinderCallbackScope.push]/[BinderCallbackScope.finish]) and return a function that
 * unregisters it. The unregister runs under [NonCancellable] when collection ends, so the callback is
 * always removed even on cancellation — which is what lets the service release its (possibly
 * late-collected) Binder GC root on the callback instead of holding it past the collector's lifetime.
 * The callback only ever references the channel, so the heavier collector graph stays collectable
 * independently.
 */
fun <T> binderCallbackFlow(
    name: String,
    register: suspend BinderCallbackScope<T>.() -> (suspend () -> Unit),
) = channelFlow {
    val callbackScope = BinderCallbackScope(name, this)
    var unregister: (suspend () -> Unit)? = null
    try {
        unregister = callbackScope.register()
        awaitClose()
    } finally {
        withContext(NonCancellable) { unregister?.invoke() }
    }
}.buffer(Channel.UNLIMITED)
