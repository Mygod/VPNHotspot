package be.mygod.vpnhotspot.root

import android.net.wifi.SoftApConfiguration
import android.os.Build
import android.os.IBinder
import androidx.annotation.RequiresApi
import androidx.collection.mutableScatterMapOf
import be.mygod.librootkotlinx.ParcelableBoolean
import be.mygod.librootkotlinx.RootCommand
import be.mygod.librootkotlinx.RootCommandNoResult
import be.mygod.librootkotlinx.RootFlow
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.util.binderCallbackFlow
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.SendChannel
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.flow.emitAll
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import timber.log.Timber

object WifiApCommands {
    private enum class SoftApCallbackCapability { Unknown, Available, Unavailable }
    private var binderSoftApCallbackCapability = SoftApCallbackCapability.Unknown
    private data class AutoFiringCallbacks(
        var state: WifiApManager.Event.OnStateChanged? = null,
        var numClients: WifiApManager.Event.OnNumClientsChanged? = null,
        var connectedClients: WifiApManager.Event.OnConnectedClientsChanged? = null,
        var info: WifiApManager.Event.OnInfoChanged? = null,
        var capability: WifiApManager.Event.OnCapabilityChanged? = null,
    ) {
        fun update(event: WifiApManager.Event) {
            when (event) {
                is WifiApManager.Event.OnStateChanged -> state = event
                is WifiApManager.Event.OnNumClientsChanged -> numClients = event
                is WifiApManager.Event.OnConnectedClientsChanged -> connectedClients = event
                is WifiApManager.Event.OnInfoChanged -> info = event
                is WifiApManager.Event.OnCapabilityChanged -> capability = event
                is WifiApManager.Event.OnBlockedClientConnecting,
                is WifiApManager.Event.OnClientsDisconnected -> { }
            }
        }

        fun sendTo(subscriber: SendChannel<WifiApManager.Event>) {
            state?.let { subscriber.trySend(it) }
            numClients?.let { subscriber.trySend(it) }
            connectedClients?.let { subscriber.trySend(it) }
            info?.let { subscriber.trySend(it) }
            capability?.let { subscriber.trySend(it) }
        }
    }
    private val softApCallbackScope = CoroutineScope(SupervisorJob())
    private val softApCallbackLock = Any()
    private val softApCallbackSubscribers = mutableScatterMapOf<SendChannel<WifiApManager.Event>, Boolean>()
    private val lastSoftApCallback = AutoFiringCallbacks()
    private var softApCallbackJob: Job? = null

    fun softApCallbackFlow(expensive: Boolean = false) = callbackFlow {
        var jobToStart: Job? = null
        synchronized(softApCallbackLock) {
            if (softApCallbackJob == null) {
                softApCallbackSubscribers[this] = expensive
                val job = softApCallbackScope.launch(start = CoroutineStart.LAZY) {
                    try {
                        collectSoftApCallbackTiers()
                    } catch (e: CancellationException) {
                        throw e
                    } catch (e: Exception) {
                        synchronized(softApCallbackLock) { softApCallbackSubscribers.forEachKey { it.close(e) } }
                    } finally {
                        val currentJob = currentCoroutineContext()[Job]
                        synchronized(softApCallbackLock) {
                            if (softApCallbackJob === currentJob) softApCallbackJob = null
                        }
                    }
                }
                softApCallbackJob = job
                jobToStart = job
            } else {
                softApCallbackSubscribers[this] = expensive
                lastSoftApCallback.sendTo(this)
            }
        }
        jobToStart?.start()
        awaitClose {
            synchronized(softApCallbackLock) {
                softApCallbackSubscribers.remove(this)
                if (softApCallbackSubscribers.isEmpty()) {
                    softApCallbackJob?.cancel()
                    softApCallbackJob = null
                }
            }
        }
    }.buffer(Channel.UNLIMITED)

    @Parcelize
    class SoftApCallbackFlow : RootFlow<WifiApManager.Event> {
        override fun flow() = WifiApManager.softApCallbackFlow
    }
    private suspend fun collectSoftApCallbackTiers(flow: Flow<WifiApManager.Event>) = try {
        flow.collect { event ->
            synchronized(softApCallbackLock) {
                lastSoftApCallback.update(event)
                softApCallbackSubscribers.removeIf { subscriber, _ -> subscriber.trySend(event).isFailure }
                if (softApCallbackSubscribers.isEmpty()) {
                    softApCallbackJob?.cancel()
                    softApCallbackJob = null
                }
            }
        }
        null
    } catch (e: CancellationException) {
        throw e
    } catch (e: WifiApManager.SoftApCallbackUnavailableException) {
        e
    } catch (e: Exception) {
        Timber.w(e)
        WifiApManager.SoftApCallbackUnavailableException(e)
    }
    private suspend fun collectSoftApCallbackTiers() {
        var failure = collectSoftApCallbackTiers(WifiApManager.softApCallbackFlow) ?: return
        if (Build.VERSION.SDK_INT >= 31) {
            failure = (collectSoftApCallbackTiers(softApCallbackBinderFlow) ?: return).apply {
                addSuppressed(failure)
            }
        }
        if (synchronized(softApCallbackLock) { softApCallbackSubscribers.any { _, expensive -> expensive } }) {
            failure = (collectSoftApCallbackTiers(flow {
                RootManager.use { emitAll(it.flow(SoftApCallbackFlow())) }
            }) ?: return).apply { addSuppressed(failure) }
        }
        throw failure
    }

    @Parcelize
    @RequiresApi(31)
    data class RegisterSoftApCallbackBinder(val callback: IBinder) : RootCommandNoResult {
        override suspend fun execute() = null.also { WifiApManager.registerSoftApCallbackBinder(callback) }
    }
    @Parcelize
    @RequiresApi(31)
    data class UnregisterSoftApCallbackBinder(val callback: IBinder) : RootCommandNoResult {
        override suspend fun execute() = null.also { WifiApManager.unregisterSoftApCallbackBinder(callback) }
    }
    @RequiresApi(31)
    private val softApCallbackBinderFlow = binderCallbackFlow("Soft AP binder callback") {
        if (binderSoftApCallbackCapability == SoftApCallbackCapability.Unavailable) {
            throw WifiApManager.SoftApCallbackUnavailableException()
        }
        val callback = try {
            WifiApManager.newSoftApCallbackBinder(WifiApManager.softApCallback(::push))
        } catch (e: ReflectiveOperationException) {
            binderSoftApCallbackCapability = SoftApCallbackCapability.Unavailable
            throw WifiApManager.SoftApCallbackUnavailableException(e)
        } catch (e: SecurityException) {
            binderSoftApCallbackCapability = SoftApCallbackCapability.Unavailable
            throw WifiApManager.SoftApCallbackUnavailableException(e)
        } catch (e: ClassCastException) {
            binderSoftApCallbackCapability = SoftApCallbackCapability.Unavailable
            throw WifiApManager.SoftApCallbackUnavailableException(e)
        } catch (e: LinkageError) {
            binderSoftApCallbackCapability = SoftApCallbackCapability.Unavailable
            throw WifiApManager.SoftApCallbackUnavailableException(e)
        }
        var registered = false
        try {
            RootManager.use { root ->
                withContext(NonCancellable) {
                    root.execute(RegisterSoftApCallbackBinder(callback))
                    registered = true
                }
            }
            binderSoftApCallbackCapability = SoftApCallbackCapability.Available
        } catch (e: CancellationException) {
            if (registered) RootManager.use { it.execute(UnregisterSoftApCallbackBinder(callback)) }
            throw e
        } catch (e: Exception) {
            throw WifiApManager.SoftApCallbackUnavailableException(e)
        }
        return@binderCallbackFlow {
            try {
                if (registered) RootManager.use { it.execute(UnregisterSoftApCallbackBinder(callback)) }
            } catch (_: CancellationException) {
            } catch (e: Exception) {
                Timber.w(e)
            }
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
    class StartLocalOnlyHotspot : RootFlow<WifiApManager.LocalOnlyHotspotEvent> {
        override fun flow() = WifiApManager.startLocalOnlyHotspotFlow(WifiApManager.configuration)
    }
}
