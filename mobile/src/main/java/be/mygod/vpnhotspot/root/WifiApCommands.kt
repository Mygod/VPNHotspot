package be.mygod.vpnhotspot.root

import android.net.wifi.SoftApConfiguration
import android.os.Build
import android.os.IBinder
import androidx.annotation.RequiresApi
import androidx.collection.mutableScatterMapOf
import be.mygod.librootkotlinx.NoShellException
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
    private var binderLocalOnlyHotspotSoftApCallbackCapability = SoftApCallbackCapability.Unknown
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
    private class SoftApCallbackRelay(
        private val collectTiers: suspend SoftApCallbackRelay.() -> Unit,
    ) {
        private val scope = CoroutineScope(SupervisorJob())
        private val lock = Any()
        private val subscribers = mutableScatterMapOf<SendChannel<WifiApManager.Event>, Boolean>()
        private val lastCallback = AutoFiringCallbacks()
        private var job: Job? = null

        fun flow(expensive: Boolean = false) = callbackFlow {
            var jobToStart: Job? = null
            synchronized(lock) {
                if (job == null) {
                    subscribers[this] = expensive
                    val startedJob = scope.launch(start = CoroutineStart.LAZY) {
                        try {
                            collectTiers()
                        } catch (e: CancellationException) {
                            throw e
                        } catch (e: Exception) {
                            synchronized(lock) { subscribers.forEachKey { it.close(e) } }
                        } finally {
                            val currentJob = currentCoroutineContext()[Job]
                            synchronized(lock) {
                                if (job === currentJob) job = null
                            }
                        }
                    }
                    job = startedJob
                    jobToStart = startedJob
                } else {
                    subscribers[this] = expensive
                    lastCallback.sendTo(this)
                }
            }
            jobToStart?.start()
            awaitClose {
                synchronized(lock) {
                    subscribers.remove(this)
                    if (subscribers.isEmpty()) {
                        job?.cancel()
                        job = null
                    }
                }
            }
        }.buffer(Channel.UNLIMITED)

        suspend fun collect(flow: Flow<WifiApManager.Event>) = try {
            flow.collect { event ->
                synchronized(lock) {
                    lastCallback.update(event)
                    subscribers.removeIf { subscriber, _ -> subscriber.trySend(event).isFailure }
                    if (subscribers.isEmpty()) {
                        job?.cancel()
                        job = null
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

        fun hasExpensiveSubscribers() = synchronized(lock) { subscribers.any { _, expensive -> expensive } }
    }

    private val softApCallbackRelay = SoftApCallbackRelay {
        var failure = collect(WifiApManager.softApCallbackFlow) ?: return@SoftApCallbackRelay
        if (Build.VERSION.SDK_INT >= 31) {
            failure = (collect(softApCallbackBinderFlow) ?: return@SoftApCallbackRelay).apply {
                (cause as? NoShellException)?.let {
                    it.addSuppressed(failure)
                    throw it
                }
                addSuppressed(failure)
            }
        }
        if (hasExpensiveSubscribers()) failure = (collect(flow {
            RootManager.use { emitAll(it.flow(SoftApCallbackFlow())) }
        }) ?: return@SoftApCallbackRelay).apply { addSuppressed(failure) }
        throw failure
    }
    fun softApCallbackFlow(expensive: Boolean = false) = softApCallbackRelay.flow(expensive)

    @get:RequiresApi(33)
    private val localOnlyHotspotSoftApCallbackRelay = SoftApCallbackRelay {
        var failure = collect(WifiApManager.localOnlyHotspotSoftApCallbackFlow) ?: return@SoftApCallbackRelay
        failure = (collect(localOnlyHotspotSoftApCallbackBinderFlow) ?: return@SoftApCallbackRelay).apply {
            (cause as? NoShellException)?.let {
                it.addSuppressed(failure)
                throw it
            }
            addSuppressed(failure)
        }
        if (hasExpensiveSubscribers()) failure = (collect(flow {
            RootManager.use { emitAll(it.flow(LocalOnlyHotspotSoftApCallbackFlow())) }
        }) ?: return@SoftApCallbackRelay).apply { addSuppressed(failure) }
        throw failure
    }
    @RequiresApi(33)
    fun localOnlyHotspotSoftApCallbackFlow(expensive: Boolean = false) =
        localOnlyHotspotSoftApCallbackRelay.flow(expensive)

    @Parcelize
    class SoftApCallbackFlow : RootFlow<WifiApManager.Event> {
        override fun flow() = WifiApManager.softApCallbackFlow
    }
    @Parcelize
    @RequiresApi(33)
    class LocalOnlyHotspotSoftApCallbackFlow : RootFlow<WifiApManager.Event> {
        override fun flow() = WifiApManager.localOnlyHotspotSoftApCallbackFlow
    }

    @Parcelize
    @RequiresApi(31)
    data class RegisterSoftApCallback(val callback: IBinder) : RootCommandNoResult {
        override suspend fun execute() = null.also { WifiApManager.registerSoftApCallback(callback) }
    }
    @Parcelize
    @RequiresApi(31)
    data class UnregisterSoftApCallback(val callback: IBinder) : RootCommandNoResult {
        override suspend fun execute() = null.also { WifiApManager.unregisterSoftApCallback(callback) }
    }
    @Parcelize
    @RequiresApi(33)
    data class RegisterLocalOnlyHotspotSoftApCallback(val callback: IBinder) : RootCommandNoResult {
        override suspend fun execute() = null.also { WifiApManager.registerLocalOnlyHotspotSoftApCallback(callback) }
    }
    @Parcelize
    @RequiresApi(33)
    data class UnregisterLocalOnlyHotspotSoftApCallback(val callback: IBinder) : RootCommandNoResult {
        override suspend fun execute() = null.also { WifiApManager.unregisterLocalOnlyHotspotSoftApCallback(callback) }
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
                    root.execute(RegisterSoftApCallback(callback))
                    registered = true
                }
            }
            binderSoftApCallbackCapability = SoftApCallbackCapability.Available
        } catch (e: CancellationException) {
            withContext(NonCancellable) {
                if (registered) RootManager.use { it.execute(UnregisterSoftApCallback(callback)) }
            }
            throw e
        } catch (e: Exception) {
            throw WifiApManager.SoftApCallbackUnavailableException(e)
        }
        return@binderCallbackFlow {
            try {
                if (registered) RootManager.use { it.execute(UnregisterSoftApCallback(callback)) }
            } catch (_: CancellationException) {
            } catch (e: Exception) {
                Timber.w(e)
            }
        }
    }
    @RequiresApi(33)
    private val localOnlyHotspotSoftApCallbackBinderFlow =
        binderCallbackFlow("local-only hotspot Soft AP binder callback") {
            if (binderLocalOnlyHotspotSoftApCallbackCapability == SoftApCallbackCapability.Unavailable) {
                throw WifiApManager.SoftApCallbackUnavailableException()
            }
            val callback = try {
                WifiApManager.newLocalOnlyHotspotSoftApCallbackBinder(WifiApManager.softApCallback(::push))
            } catch (e: ReflectiveOperationException) {
                binderLocalOnlyHotspotSoftApCallbackCapability = SoftApCallbackCapability.Unavailable
                throw WifiApManager.SoftApCallbackUnavailableException(e)
            } catch (e: SecurityException) {
                binderLocalOnlyHotspotSoftApCallbackCapability = SoftApCallbackCapability.Unavailable
                throw WifiApManager.SoftApCallbackUnavailableException(e)
            } catch (e: ClassCastException) {
                binderLocalOnlyHotspotSoftApCallbackCapability = SoftApCallbackCapability.Unavailable
                throw WifiApManager.SoftApCallbackUnavailableException(e)
            } catch (e: LinkageError) {
                binderLocalOnlyHotspotSoftApCallbackCapability = SoftApCallbackCapability.Unavailable
                throw WifiApManager.SoftApCallbackUnavailableException(e)
            }
            var registered = false
            try {
                RootManager.use { root ->
                    withContext(NonCancellable) {
                        root.execute(RegisterLocalOnlyHotspotSoftApCallback(callback))
                        registered = true
                    }
                }
                binderLocalOnlyHotspotSoftApCallbackCapability = SoftApCallbackCapability.Available
            } catch (e: CancellationException) {
                withContext(NonCancellable) {
                    if (registered) RootManager.use {
                        it.execute(UnregisterLocalOnlyHotspotSoftApCallback(callback))
                    }
                }
                throw e
            } catch (e: Exception) {
                throw WifiApManager.SoftApCallbackUnavailableException(e)
            }
            return@binderCallbackFlow {
                try {
                    if (registered) RootManager.use {
                        it.execute(UnregisterLocalOnlyHotspotSoftApCallback(callback))
                    }
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
