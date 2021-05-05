package be.mygod.vpnhotspot.root

import android.net.MacAddress
import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.ParcelableBoolean
import be.mygod.librootkotlinx.RootCommand
import be.mygod.librootkotlinx.RootCommandChannel
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.*
import kotlinx.parcelize.Parcelize
import timber.log.Timber

object WifiApCommands {
    @RequiresApi(28)
    sealed class SoftApCallbackParcel : Parcelable {
        abstract fun dispatch(callback: WifiApManager.SoftApCallbackCompat)

        @Parcelize
        data class OnStateChanged(val state: Int, val failureReason: Int) : SoftApCallbackParcel() {
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) =
                    callback.onStateChanged(state, failureReason)
        }
        @Parcelize
        data class OnNumClientsChanged(val numClients: Int) : SoftApCallbackParcel() {
            @Suppress("DEPRECATION")
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) =
                    callback.onNumClientsChanged(numClients)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnConnectedClientsChanged(val clients: List<MacAddress>) : SoftApCallbackParcel() {
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) =
                    callback.onConnectedClientsChanged(clients)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnInfoChanged(val frequency: Int, val bandwidth: Int) : SoftApCallbackParcel() {
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) =
                    callback.onInfoChanged(frequency, bandwidth)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnCapabilityChanged(val maxSupportedClients: Int,
                                       val supportedFeatures: Long) : SoftApCallbackParcel() {
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) =
                    callback.onCapabilityChanged(maxSupportedClients, supportedFeatures)
        }
        @Parcelize
        @RequiresApi(30)
        data class OnBlockedClientConnecting(val client: MacAddress, val blockedReason: Int) : SoftApCallbackParcel() {
            override fun dispatch(callback: WifiApManager.SoftApCallbackCompat) =
                    callback.onBlockedClientConnecting(client, blockedReason)
        }
    }

    @Parcelize
    @RequiresApi(28)
    class RegisterSoftApCallback : RootCommandChannel<SoftApCallbackParcel> {
        override fun create(scope: CoroutineScope) = scope.produce<SoftApCallbackParcel>(capacity = capacity) {
            val finish = CompletableDeferred<Unit>()
            val key = WifiApManager.registerSoftApCallback(object : WifiApManager.SoftApCallbackCompat {
                private fun push(parcel: SoftApCallbackParcel) {
                    trySend(parcel).onClosed {
                        finish.completeExceptionally(it ?: ClosedSendChannelException("Channel was closed normally"))
                    }.onFailure { throw it!! }
                }

                override fun onStateChanged(state: Int, failureReason: Int) =
                        push(SoftApCallbackParcel.OnStateChanged(state, failureReason))
                @Suppress("OverridingDeprecatedMember")
                override fun onNumClientsChanged(numClients: Int) =
                        push(SoftApCallbackParcel.OnNumClientsChanged(numClients))
                @RequiresApi(30)
                override fun onConnectedClientsChanged(clients: List<MacAddress>) =
                        push(SoftApCallbackParcel.OnConnectedClientsChanged(clients))
                @RequiresApi(30)
                override fun onInfoChanged(frequency: Int, bandwidth: Int) =
                        push(SoftApCallbackParcel.OnInfoChanged(frequency, bandwidth))
                @RequiresApi(30)
                override fun onCapabilityChanged(maxSupportedClients: Int, supportedFeatures: Long) =
                        push(SoftApCallbackParcel.OnCapabilityChanged(maxSupportedClients, supportedFeatures))
                @RequiresApi(30)
                override fun onBlockedClientConnecting(client: MacAddress, blockedReason: Int) =
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
    @RequiresApi(28)
    private suspend fun handleChannel(channel: ReceiveChannel<SoftApCallbackParcel>) = channel.consumeEach { parcel ->
        when (parcel) {
            is SoftApCallbackParcel.OnStateChanged -> synchronized(callbacks) { lastCallback.state = parcel }
            is SoftApCallbackParcel.OnNumClientsChanged -> synchronized(callbacks) { lastCallback.numClients = parcel }
            is SoftApCallbackParcel.OnConnectedClientsChanged -> synchronized(callbacks) {
                lastCallback.connectedClients = parcel
            }
            is SoftApCallbackParcel.OnInfoChanged -> synchronized(callbacks) { lastCallback.info = parcel }
            is SoftApCallbackParcel.OnCapabilityChanged -> synchronized(callbacks) { lastCallback.capability = parcel }
        }
        for (callback in synchronized(callbacks) { callbacks }) parcel.dispatch(callback)
    }
    @RequiresApi(28)
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
            lastCallback
        } else null
    }?.toSequence()?.forEach { it?.dispatch(callback) }
    @RequiresApi(28)
    fun unregisterSoftApCallback(callback: WifiApManager.SoftApCallbackCompat) = synchronized(callbacks) {
        if (callbacks.remove(callback) && callbacks.isEmpty()) {
            rootCallbackJob!!.cancel()
            rootCallbackJob = null
        }
    }

    @Parcelize
    class GetConfiguration : RootCommand<SoftApConfigurationCompat> {
        override suspend fun execute() = WifiApManager.configuration
    }

    @Parcelize
    data class SetConfiguration(val configuration: SoftApConfigurationCompat) : RootCommand<ParcelableBoolean> {
        override suspend fun execute() = ParcelableBoolean(WifiApManager.setConfiguration(configuration))
    }
}
