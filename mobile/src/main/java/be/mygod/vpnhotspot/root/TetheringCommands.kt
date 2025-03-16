package be.mygod.vpnhotspot.root

import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.RootCommandChannel
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.channels.ClosedSendChannelException
import kotlinx.coroutines.channels.onClosed
import kotlinx.coroutines.channels.onFailure
import kotlinx.coroutines.channels.produce
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize

object TetheringCommands {
    /**
     * This is the only command supported since other callbacks do not require signature permissions.
     */
    @Parcelize
    data class OnClientsChanged(val clients: List<Parcelable>) : Parcelable {
        fun dispatch(callback: TetheringManagerCompat.TetheringEventCallback) = callback.onClientsChanged(clients)
    }

    @Parcelize
    @RequiresApi(30)
    class RegisterTetheringEventCallback : RootCommandChannel<OnClientsChanged> {
        override fun create(scope: CoroutineScope) = scope.produce(capacity = capacity) {
            val finish = CompletableDeferred<Unit>()
            val callback = object : TetheringManagerCompat.TetheringEventCallback {
                private fun push(parcel: OnClientsChanged) {
                    trySend(parcel).onClosed {
                        finish.completeExceptionally(it ?: ClosedSendChannelException("Channel was closed normally"))
                        return
                    }.onFailure { throw it!! }
                }

                override fun onClientsChanged(clients: Collection<Parcelable>) =
                    push(OnClientsChanged(clients.toList()))
            }
            TetheringManagerCompat.registerTetheringEventCallback(callback) {
                scope.launch {
                    try {
                        it.run()
                    } catch (e: Throwable) {
                        finish.completeExceptionally(e)
                    }
                }
            }
            try {
                finish.await()
            } finally {
                TetheringManagerCompat.unregisterTetheringEventCallback(callback)
            }
        }
    }
}
