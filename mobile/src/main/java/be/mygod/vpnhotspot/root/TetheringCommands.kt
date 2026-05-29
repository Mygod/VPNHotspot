package be.mygod.vpnhotspot.root

import android.net.TetheredClient
import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.RootCommandNoResult
import be.mygod.librootkotlinx.RootFlow
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.util.Services
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.channels.onFailure
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize

object TetheringCommands {
    @Parcelize
    @RequiresApi(30)
    data class Start(private val type: Int, private val showProvisioningUi: Boolean) : RootCommandNoResult {
        override suspend fun execute() = null.also {
            TetheringManagerCompat.startTethering(type, true, showProvisioningUi)
        }
    }
    @Parcelize
    @RequiresApi(30)
    data class Stop(private val type: Int) : RootCommandNoResult {
        override suspend fun execute() = null.also { TetheringManagerCompat.stopTethering(type, Services.context) }
    }
    @Deprecated("Old API since API 30")
    @Parcelize
    @Suppress("DEPRECATION")
    data class StartLegacy(private val type: Int, private val showProvisioningUi: Boolean) : RootCommandNoResult {
        override suspend fun execute() = null.also {
            TetheringManagerCompat.startTetheringLegacy(type, showProvisioningUi)
        }
    }
    @Parcelize
    data class StopLegacy(private val type: Int) : RootCommandNoResult {
        override suspend fun execute() = null.also { TetheringManagerCompat.stopTetheringLegacy(type) }
    }

    /**
     * This is the only command supported since other callbacks do not require signature permissions.
     */
    @Parcelize
    data class OnClientsChanged(val clients: List<TetheredClient>) : Parcelable {
        fun dispatch(callback: TetheringManagerCompat.TetheringEventCallback) = callback.onClientsChanged(clients)
    }

    @Parcelize
    @RequiresApi(30)
    class TetheringEventCallbackFlow : RootFlow<OnClientsChanged> {
        override fun flow() = callbackFlow {
            val callback = object : TetheringManagerCompat.TetheringEventCallback {
                private fun push(parcel: OnClientsChanged) {
                    trySend(parcel).onFailure {
                        close(it ?: IllegalStateException("Flow buffer rejected tethering event"))
                    }
                }

                override fun onClientsChanged(clients: Collection<TetheredClient>) =
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
