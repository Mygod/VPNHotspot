package be.mygod.vpnhotspot.root

import android.net.wifi.SoftApConfiguration
import android.net.wifi.SoftApCapability
import android.net.wifi.SoftApInfo
import android.net.wifi.WifiManager
import android.net.wifi.WifiClient
import android.net.wifi.`WifiManager$SoftApCallback`
import android.os.Build
import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.ParcelableBoolean
import be.mygod.librootkotlinx.RootCommand
import be.mygod.librootkotlinx.RootFlow
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.util.UnblockCentral
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
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize
import timber.log.Timber

object WifiApCommands {
    sealed class SoftApCallbackParcel : Parcelable {
        abstract fun dispatch(callback: `WifiManager$SoftApCallback`)

        @Parcelize
        data class OnStateChanged(val state: Int, val failureReason: Int) : SoftApCallbackParcel() {
            override fun dispatch(callback: `WifiManager$SoftApCallback`) {
                if (Build.VERSION.SDK_INT == 29) UnblockCentral.SoftApCallback.onStateChanged
                callback.onStateChanged(state, failureReason)
            }
        }
        @Parcelize
        data class OnNumClientsChanged(val numClients: Int) : SoftApCallbackParcel() {
            override fun dispatch(callback: `WifiManager$SoftApCallback`) {
                if (Build.VERSION.SDK_INT == 29) UnblockCentral.SoftApCallback.onNumClientsChanged
                callback.onNumClientsChanged(numClients)
            }
        }
        @Parcelize
        @RequiresApi(30)
        data class OnConnectedClientsChanged(val clients: List<WifiClient>) : SoftApCallbackParcel() {
            override fun dispatch(callback: `WifiManager$SoftApCallback`) = callback.onConnectedClientsChanged(clients)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnInfoChanged(val info: SoftApInfo) : SoftApCallbackParcel() {
            override fun dispatch(callback: `WifiManager$SoftApCallback`) = callback.onInfoChanged(info)
        }
        @Parcelize
        @RequiresApi(31)
        data class OnInfoListChanged(val infos: List<SoftApInfo>) : SoftApCallbackParcel() {
            override fun dispatch(callback: `WifiManager$SoftApCallback`) = callback.onInfoChanged(infos)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnCapabilityChanged(val capability: SoftApCapability) : SoftApCallbackParcel() {
            override fun dispatch(callback: `WifiManager$SoftApCallback`) = callback.onCapabilityChanged(capability)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnBlockedClientConnecting(
            val client: WifiClient,
            val blockedReason: Int,
        ) : SoftApCallbackParcel() {
            override fun dispatch(callback: `WifiManager$SoftApCallback`) =
                callback.onBlockedClientConnecting(client, blockedReason)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnClientsDisconnected(
            val info: SoftApInfo,
            val clients: List<WifiClient>,
        ) : SoftApCallbackParcel() {
            override fun dispatch(callback: `WifiManager$SoftApCallback`) =
                callback.onClientsDisconnected(info, clients)
        }
    }

    @Parcelize
    class RegisterSoftApCallback : RootFlow<SoftApCallbackParcel> {
        override fun flow() = callbackFlow {
            val key = WifiApManager.registerSoftApCallback(object : `WifiManager$SoftApCallback` {
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
                override fun onConnectedClientsChanged(clients: List<WifiClient>) =
                    push(SoftApCallbackParcel.OnConnectedClientsChanged(clients))
                @RequiresApi(30)
                override fun onInfoChanged(info: SoftApInfo) = push(SoftApCallbackParcel.OnInfoChanged(info))
                @RequiresApi(31)
                override fun onInfoChanged(info: List<SoftApInfo>) =
                    push(SoftApCallbackParcel.OnInfoListChanged(info))
                @RequiresApi(30)
                override fun onCapabilityChanged(capability: SoftApCapability) =
                    push(SoftApCallbackParcel.OnCapabilityChanged(capability))
                @RequiresApi(30)
                override fun onBlockedClientConnecting(client: WifiClient, blockedReason: Int) =
                    push(SoftApCallbackParcel.OnBlockedClientConnecting(client, blockedReason))
                override fun onClientsDisconnected(info: SoftApInfo, clients: List<WifiClient>) =
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
            var info: SoftApCallbackParcel? = null,
            var capability: SoftApCallbackParcel.OnCapabilityChanged? = null) {
        fun toSequence() = sequenceOf(state, numClients, connectedClients, info, capability)
    }

    private val callbacks = mutableSetOf<`WifiManager$SoftApCallback`>()
    private val lastCallback = AutoFiringCallbacks()
    private var rootCallbackJob: Job? = null
    private suspend fun handleFlow(flow: Flow<SoftApCallbackParcel>) = flow.collect { parcel ->
        when (parcel) {
            is SoftApCallbackParcel.OnStateChanged -> synchronized(callbacks) { lastCallback.state = parcel }
            is SoftApCallbackParcel.OnNumClientsChanged -> synchronized(callbacks) { lastCallback.numClients = parcel }
            is SoftApCallbackParcel.OnConnectedClientsChanged -> synchronized(callbacks) {
                lastCallback.connectedClients = parcel
            }
            is SoftApCallbackParcel.OnInfoChanged, is SoftApCallbackParcel.OnInfoListChanged -> {
                synchronized(callbacks) { lastCallback.info = parcel }
            }
            is SoftApCallbackParcel.OnCapabilityChanged -> synchronized(callbacks) { lastCallback.capability = parcel }
            // do nothing for one-time events
            is SoftApCallbackParcel.OnBlockedClientConnecting, is SoftApCallbackParcel.OnClientsDisconnected -> { }
        }
        for (callback in synchronized(callbacks) { callbacks.toList() }) parcel.dispatch(callback)
    }
    fun registerSoftApCallback(callback: `WifiManager$SoftApCallback`) = synchronized(callbacks) {
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
    fun unregisterSoftApCallback(callback: `WifiManager$SoftApCallback`) = synchronized(callbacks) {
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
