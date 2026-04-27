package be.mygod.vpnhotspot.root

import android.net.wifi.SoftApConfiguration
import android.net.wifi.WifiManager
import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.ParcelableBoolean
import be.mygod.librootkotlinx.RootCommand
import be.mygod.librootkotlinx.RootFlow
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.channels.onFailure
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize
import timber.log.Timber

object WifiApCommands {
    sealed class SoftApCallbackParcel : Parcelable {
        abstract fun dispatch(callback: WifiApManager.SoftApCallbackCompat)

        @Parcelize
        data class OnStateChanged(val state: Int, val failureReason: Int) : SoftApCallbackParcel() {
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) =
                    callback.onStateChanged(state, failureReason)
        }
        @Parcelize
        data class OnNumClientsChanged(val numClients: Int) : SoftApCallbackParcel() {
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) =
                    callback.onNumClientsChanged(numClients)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnConnectedClientsChanged(val clients: List<Parcelable>) : SoftApCallbackParcel() {
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) =
                    callback.onConnectedClientsChanged(clients)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnInfoChanged(val info: List<Parcelable>) : SoftApCallbackParcel() {
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) = callback.onInfoChanged(info)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnCapabilityChanged(val capability: Parcelable) : SoftApCallbackParcel() {
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) =
                    callback.onCapabilityChanged(capability)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnBlockedClientConnecting(val client: Parcelable, val blockedReason: Int) : SoftApCallbackParcel() {
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) =
                callback.onBlockedClientConnecting(client, blockedReason)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnClientsDisconnected(val info: Parcelable, val clients: List<Parcelable>) : SoftApCallbackParcel() {
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) =
                callback.onClientsDisconnected(info, clients)
        }
    }

    @Parcelize
    class RegisterSoftApCallback : RootFlow<SoftApCallbackParcel> {
        override fun flow() = callbackFlow {
            val key = WifiApManager.registerSoftApCallback(object : WifiApManager.SoftApCallbackCompat {
                private fun push(parcel: SoftApCallbackParcel) {
                    trySend(parcel).onFailure {
                        close(it ?: IllegalStateException("Flow buffer rejected Soft AP callback"))
                    }
                }

                override fun onStateChanged(state: Int, failureReason: Int) =
                    push(SoftApCallbackParcel.OnStateChanged(state, failureReason))
                override fun onNumClientsChanged(numClients: Int) =
                    push(SoftApCallbackParcel.OnNumClientsChanged(numClients))
                @RequiresApi(30)
                override fun onConnectedClientsChanged(clients: List<Parcelable>) =
                    push(SoftApCallbackParcel.OnConnectedClientsChanged(clients))
                @RequiresApi(30)
                override fun onInfoChanged(info: List<Parcelable>) = push(SoftApCallbackParcel.OnInfoChanged(info))
                @RequiresApi(30)
                override fun onCapabilityChanged(capability: Parcelable) =
                    push(SoftApCallbackParcel.OnCapabilityChanged(capability))
                @RequiresApi(30)
                override fun onBlockedClientConnecting(client: Parcelable, blockedReason: Int) =
                    push(SoftApCallbackParcel.OnBlockedClientConnecting(client, blockedReason))
                override fun onClientsDisconnected(info: Parcelable, clients: List<Parcelable>) =
                    push(SoftApCallbackParcel.OnClientsDisconnected(info, clients))
            }) {
                launch {
                    try {
                        it.run()
                    } catch (e: Throwable) {
                        close(e)
                    }
                }
            }
            awaitClose {
                WifiApManager.unregisterSoftApCallback(key)
            }
        }.buffer(Channel.UNLIMITED)
    }

    private data class AutoFiringCallbacks(
            var state: SoftApCallbackParcel.OnStateChanged? = null,
            var numClients: SoftApCallbackParcel.OnNumClientsChanged? = null,
            var connectedClients: SoftApCallbackParcel.OnConnectedClientsChanged? = null,
            var info: SoftApCallbackParcel.OnInfoChanged? = null,
            var capability: SoftApCallbackParcel.OnCapabilityChanged? = null) {
        fun toSequence() = sequenceOf(state, numClients, connectedClients, info, capability)
    }

    private val callbacks = mutableSetOf<WifiApManager.SoftApCallbackCompat>()
    private val lastCallback = AutoFiringCallbacks()
    private var rootCallbackJob: Job? = null
    private suspend fun handleFlow(flow: Flow<SoftApCallbackParcel>) = flow.collect { parcel ->
        when (parcel) {
            is SoftApCallbackParcel.OnStateChanged -> synchronized(callbacks) { lastCallback.state = parcel }
            is SoftApCallbackParcel.OnNumClientsChanged -> synchronized(callbacks) { lastCallback.numClients = parcel }
            is SoftApCallbackParcel.OnConnectedClientsChanged -> synchronized(callbacks) {
                lastCallback.connectedClients = parcel
            }
            is SoftApCallbackParcel.OnInfoChanged -> synchronized(callbacks) { lastCallback.info = parcel }
            is SoftApCallbackParcel.OnCapabilityChanged -> synchronized(callbacks) { lastCallback.capability = parcel }
            // do nothing for one-time events
            is SoftApCallbackParcel.OnBlockedClientConnecting, is SoftApCallbackParcel.OnClientsDisconnected -> { }
        }
        for (callback in synchronized(callbacks) { callbacks.toList() }) parcel.dispatch(callback)
    }
    fun registerSoftApCallback(callback: WifiApManager.SoftApCallbackCompat) = synchronized(callbacks) {
        val wasEmpty = callbacks.isEmpty()
        callbacks.add(callback)
        if (wasEmpty) {
            rootCallbackJob = GlobalScope.launch {
                try {
                    RootManager.use { server -> handleFlow(server.flow(RegisterSoftApCallback())) }
                } catch (_: CancellationException) {
                } catch (e: Exception) {
                    Timber.w(e)
                    SmartSnackbar.make(e).show()
                }
            }
            null
        } else lastCallback
    }?.toSequence()?.forEach { it?.dispatch(callback) }
    fun unregisterSoftApCallback(callback: WifiApManager.SoftApCallbackCompat) = synchronized(callbacks) {
        if (callbacks.remove(callback) && callbacks.isEmpty()) {
            rootCallbackJob!!.cancel()
            rootCallbackJob = null
        }
    }

    @Parcelize
    @Deprecated("Use GetConfiguration instead", ReplaceWith("GetConfiguration"))
    @Suppress("DEPRECATION")
    class GetConfigurationLegacy : RootCommand<android.net.wifi.WifiConfiguration?> {
        override suspend fun execute() = WifiApManager.configurationLegacy
    }
    @Parcelize
    @RequiresApi(30)
    class GetConfiguration : RootCommand<SoftApConfiguration> {
        override suspend fun execute() = WifiApManager.configuration
    }

    @Parcelize
    @Deprecated("Use SetConfiguration instead", ReplaceWith("SetConfiguration"))
    @Suppress("DEPRECATION")
    data class SetConfigurationLegacy(
        val configuration: android.net.wifi.WifiConfiguration?,
    ) : RootCommand<ParcelableBoolean> {
        override suspend fun execute() = ParcelableBoolean(WifiApManager.setConfiguration(configuration))
    }
    @Parcelize
    @RequiresApi(30)
    data class SetConfiguration(val configuration: SoftApConfiguration) : RootCommand<ParcelableBoolean> {
        override suspend fun execute() = ParcelableBoolean(WifiApManager.setConfiguration(configuration))
    }

    @Parcelize
    @RequiresApi(30)
    class StartLocalOnlyHotspot : RootFlow<LocalOnlyHotspotCallbacks> {
        override fun flow() = callbackFlow {
            var lohr: WifiManager.LocalOnlyHotspotReservation? = null
            var completed = false
            WifiApManager.startLocalOnlyHotspot(WifiApManager.configuration, object :
                WifiManager.LocalOnlyHotspotCallback() {
                private fun push(parcel: LocalOnlyHotspotCallbacks) {
                    trySend(parcel).onFailure {
                        close(it ?: IllegalStateException("Flow buffer rejected local-only hotspot callback"))
                    }
                }
                override fun onStarted(reservation: WifiManager.LocalOnlyHotspotReservation?) {
                    if (reservation == null) onFailed(-3) else {
                        require(lohr == null)
                        lohr = reservation
                        push(LocalOnlyHotspotCallbacks.OnStarted(reservation.softApConfiguration))
                    }
                }
                override fun onStopped() {
                    push(LocalOnlyHotspotCallbacks.OnStopped())
                    completed = true
                    close()
                }
                override fun onFailed(reason: Int) {
                    push(LocalOnlyHotspotCallbacks.OnFailed(reason))
                    completed = true
                    close()
                }
            }) {
                launch {
                    try {
                        it.run()
                    } catch (e: Throwable) {
                        close(e)
                    }
                }
            }
            awaitClose {
                if (!completed) WifiApManager.cancelLocalOnlyHotspotRequest()
                lohr?.close()
            }
        }.buffer(Channel.UNLIMITED)
    }
}
