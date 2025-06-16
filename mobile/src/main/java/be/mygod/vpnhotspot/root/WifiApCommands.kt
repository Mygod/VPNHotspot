package be.mygod.vpnhotspot.root

import android.net.wifi.SoftApConfiguration
import android.net.wifi.WifiManager
import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.ParcelableBoolean
import be.mygod.librootkotlinx.RootCommand
import be.mygod.librootkotlinx.RootCommandChannel
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.ClosedSendChannelException
import kotlinx.coroutines.channels.ReceiveChannel
import kotlinx.coroutines.channels.consumeEach
import kotlinx.coroutines.channels.onClosed
import kotlinx.coroutines.channels.onFailure
import kotlinx.coroutines.channels.produce
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
    }

    @Parcelize
    class RegisterSoftApCallback : RootCommandChannel<SoftApCallbackParcel> {
        override fun create(scope: CoroutineScope) = scope.produce(capacity = capacity) {
            val finish = CompletableDeferred<Unit>()
            val key = WifiApManager.registerSoftApCallback(object : WifiApManager.SoftApCallbackCompat {
                private fun push(parcel: SoftApCallbackParcel) {
                    trySend(parcel).onClosed {
                        finish.completeExceptionally(it ?: ClosedSendChannelException("Channel was closed normally"))
                        return
                    }.onFailure { throw it!! }
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
            }) {
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
                WifiApManager.unregisterSoftApCallback(key)
            }
        }
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
    private suspend fun handleChannel(channel: ReceiveChannel<SoftApCallbackParcel>) = channel.consumeEach { parcel ->
        when (parcel) {
            is SoftApCallbackParcel.OnStateChanged -> synchronized(callbacks) { lastCallback.state = parcel }
            is SoftApCallbackParcel.OnNumClientsChanged -> synchronized(callbacks) { lastCallback.numClients = parcel }
            is SoftApCallbackParcel.OnConnectedClientsChanged -> synchronized(callbacks) {
                lastCallback.connectedClients = parcel
            }
            is SoftApCallbackParcel.OnInfoChanged -> synchronized(callbacks) { lastCallback.info = parcel }
            is SoftApCallbackParcel.OnCapabilityChanged -> synchronized(callbacks) { lastCallback.capability = parcel }
            // do nothing for one-time events
            is SoftApCallbackParcel.OnBlockedClientConnecting -> { }
        }
        for (callback in synchronized(callbacks) { callbacks.toList() }) parcel.dispatch(callback)
    }
    fun registerSoftApCallback(callback: WifiApManager.SoftApCallbackCompat) = synchronized(callbacks) {
        val wasEmpty = callbacks.isEmpty()
        callbacks.add(callback)
        if (wasEmpty) {
            rootCallbackJob = GlobalScope.launch {
                try {
                    RootManager.use { server -> handleChannel(server.create(RegisterSoftApCallback(), this)) }
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
    class StartLocalOnlyHotspot : RootCommandChannel<LocalOnlyHotspotCallbacks> {
        override fun create(scope: CoroutineScope) = scope.produce(capacity = capacity) {
            val finish = CompletableDeferred<Unit>()
            var lohr: WifiManager.LocalOnlyHotspotReservation? = null
            WifiApManager.startLocalOnlyHotspot(WifiApManager.configuration, object :
                WifiManager.LocalOnlyHotspotCallback() {
                private fun push(parcel: LocalOnlyHotspotCallbacks) {
                    trySend(parcel).onClosed {
                        finish.completeExceptionally(it ?: ClosedSendChannelException("Channel was closed normally"))
                        return
                    }.onFailure { throw it!! }
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
                    finish.complete(Unit)
                }
                override fun onFailed(reason: Int) {
                    push(LocalOnlyHotspotCallbacks.OnFailed(reason))
                    finish.complete(Unit)
                }
            }) {
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
            } catch (e: Exception) {
                WifiApManager.cancelLocalOnlyHotspotRequest()
                throw e
            } finally {
                lohr?.close()
            }
        }
    }
}
