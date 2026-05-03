package be.mygod.vpnhotspot.root

import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.RootFlow
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.channels.onFailure
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.callbackFlow
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
    class RegisterTetheringEventCallback : RootFlow<OnClientsChanged> {
        override fun flow() = callbackFlow {
            val callback = object : TetheringManagerCompat.TetheringEventCallback {
                private fun push(parcel: OnClientsChanged) {
                    trySend(parcel).onFailure {
                        close(it ?: IllegalStateException("Flow buffer rejected tethering event"))
                    }
                }

                override fun onClientsChanged(clients: Collection<Parcelable>) =
                    push(OnClientsChanged(clients.toList()))
            }
            TetheringManagerCompat.registerTetheringEventCallback(callback) {
                launch {
                    try {
                        it.run()
                    } catch (e: Throwable) {
                        close(e)
                    }
                }
            }
            awaitClose {
                TetheringManagerCompat.unregisterTetheringEventCallback(callback)
            }
        }.buffer(Channel.UNLIMITED)
    }
}
