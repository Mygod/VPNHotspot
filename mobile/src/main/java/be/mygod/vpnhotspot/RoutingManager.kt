package be.mygod.vpnhotspot

import android.os.Build
import androidx.collection.MutableScatterMap
import androidx.core.content.edit
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.Routing.Ipv6Mode
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.root.daemon.MasqueradeMode
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import timber.log.Timber

abstract class RoutingManager(private val caller: Any, val downstream: String, private val forceWifi: Boolean = false) {
    companion object {
        private const val KEY_MASQUERADE_MODE = "service.masqueradeMode"
        private const val KEY_IPV6_MODE = "service.ipv6Mode"
        var masqueradeMode: MasqueradeMode
            get() = app.pref.run {
                getString(KEY_MASQUERADE_MODE, null)?.let {
                    return@run when (it) {
                        "None" -> MasqueradeMode.MASQUERADE_MODE_NONE
                        "Simple" -> MasqueradeMode.MASQUERADE_MODE_SIMPLE
                        "Netd" -> MasqueradeMode.MASQUERADE_MODE_NETD
                        "MASQUERADE_MODE_NONE" -> MasqueradeMode.MASQUERADE_MODE_NONE
                        "MASQUERADE_MODE_SIMPLE" -> MasqueradeMode.MASQUERADE_MODE_SIMPLE
                        "MASQUERADE_MODE_NETD" -> MasqueradeMode.MASQUERADE_MODE_NETD
                        else -> throw IllegalArgumentException("Invalid masquerade mode $it")
                    }
                }
                if (getBoolean("service.masquerade", true)) {   // legacy settings
                    MasqueradeMode.MASQUERADE_MODE_SIMPLE
                } else MasqueradeMode.MASQUERADE_MODE_NONE
            }
            set(value) = app.pref.edit {
                putString(KEY_MASQUERADE_MODE, when (value) {
                    MasqueradeMode.MASQUERADE_MODE_NONE -> "None"
                    MasqueradeMode.MASQUERADE_MODE_SIMPLE -> "Simple"
                    MasqueradeMode.MASQUERADE_MODE_NETD -> "Netd"
                    is MasqueradeMode.Unrecognized -> throw IllegalArgumentException("Invalid masquerade mode")
                })
            }
        var ipv6Mode: Ipv6Mode
            get() = app.pref.run {
                getString(KEY_IPV6_MODE, null)?.let { return@run Ipv6Mode.valueOf(it) }
                if (getBoolean("service.disableIpv6", true)) Ipv6Mode.Block else Ipv6Mode.System
            }
            set(value) = app.pref.edit { putString(KEY_IPV6_MODE, value.name) }

        /**
         * Thread safety: needs protection by [monitor]!
         */
        private val active = MutableScatterMap<String, RoutingManager>()
        private val monitor = Mutex()
        private var cleaning: CompletableDeferred<Unit>? = null

        suspend fun clean() {
            val clean = CompletableDeferred<Unit>()
            monitor.withLock {
                cleaning?.let { return@withLock it }
                cleaning = clean
                null
            }?.let {
                it.await()
                return
            }
            try {
                val routings = monitor.withLock {
                    buildList(active.size) {
                        active.forEachValue { manager ->
                            manager.routing?.also {
                                manager.routing = null
                                add(it)
                            }
                        }
                    }
                }
                try {
                    Routing.clean()
                    for (routing in routings) routing.stopForClean()
                } catch (e: Exception) {
                    for (routing in routings) routing.revert()
                    Timber.d(e)
                    SmartSnackbar.make(e).show()
                    return
                }
                val restarts = monitor.withLock {
                    buildList(active.size) {
                        active.forEachValue { manager ->
                            if (manager.started && manager.routing == null) {
                                val routing = Routing(manager.caller, manager.downstream)
                                manager.routing = routing
                                add(manager to routing)
                            }
                        }
                    }
                }
                for ((manager, routing) in restarts) monitor.withLock {
                    if (manager.routing === routing) manager.startRoutingLocked(routing)
                }
                monitor.withLock {
                    if (cleaning === clean) cleaning = null
                }
                clean.complete(Unit)
            } finally {
                monitor.withLock {
                    if (cleaning === clean) cleaning = null
                }
                clean.complete(Unit)
            }
        }
    }

    /**
     * Both repeater and local-only hotspot are Wi-Fi based.
     */
    class LocalOnly(caller: Any, downstream: String) : RoutingManager(caller, downstream, true) {
        override fun Routing.configure() {
            ipForward = true // local only interfaces need to enable ip_forward
            ipv6Mode = when (RoutingManager.ipv6Mode) {
                Ipv6Mode.Block, Ipv6Mode.System -> Ipv6Mode.System
                Ipv6Mode.Nat -> Ipv6Mode.Nat
            }
        }
    }

    var started = false
        private set
    /**
     * Thread safety: needs protection by [monitor]!
     */
    private var routing: Routing? = null
    private var isWifi = forceWifi || TetherType.ofInterface(downstream).isWifi
    private var tetherTypeJob: Job? = null

    suspend fun start(): Boolean {
        while (true) {
            var clean: CompletableDeferred<Unit>? = null
            val routing = monitor.withLock {
                clean = cleaning
                var routing: Routing? = null
                if (clean == null) active.compute(downstream) { _, other ->
                    when (other) {
                        null -> {
                            started = true
                            acquireLocked()
                            routing = newRoutingLocked()
                            this@RoutingManager
                        }
                        this@RoutingManager -> {
                            if (started) routing = this@RoutingManager.routing ?: newRoutingLocked()
                            other
                        }
                        else -> {
                            val msg = "Double routing detected for $downstream from $caller != ${other.caller}"
                            Timber.w(RuntimeException(msg))
                            SmartSnackbar.make(msg).show()
                            other
                        }
                    }
                }
                routing
            }
            clean?.let {
                it.await()
                continue
            }
            return routing != null
        }
    }

    private fun acquireLocked() {
        if (isWifi) WifiDoubleLock.acquire(this)
        if (!forceWifi && Build.VERSION.SDK_INT >= 30) {
            tetherTypeJob = CoroutineScope(Dispatchers.Default).launch(start = CoroutineStart.UNDISPATCHED) {
                TetherType.changes.collect {
                    monitor.withLock {
                        if (active[downstream] !== this@RoutingManager || !started) return@withLock
                        val isWifiNow = TetherType.ofInterface(downstream).isWifi
                        if (isWifi != isWifiNow) {
                            if (isWifi) WifiDoubleLock.release(this@RoutingManager)
                            else WifiDoubleLock.acquire(this@RoutingManager)
                            isWifi = isWifiNow
                        }
                    }
                }
            }
        }
    }
    private fun releaseLocked() {
        if (!forceWifi && Build.VERSION.SDK_INT >= 30) {
            tetherTypeJob?.cancel()
            tetherTypeJob = null
        }
        if (isWifi) WifiDoubleLock.release(this)
    }

    private fun newRoutingLocked() = Routing(caller, downstream).also {
        routing = it
        startRoutingLocked(it)
    }
    private fun startRoutingLocked(routing: Routing) {
        routing.masqueradeMode = masqueradeMode
        routing.configure()
        routing.start()
    }

    protected abstract fun Routing.configure()

    suspend fun stop() {
        val routing = monitor.withLock {
            if (active[downstream] !== this@RoutingManager || !started && routing == null) {
                started = false
                null
            } else {
                if (started) releaseLocked()
                started = false
                routing.also {
                    routing = null
                    if (it == null) active.remove(downstream, this@RoutingManager)
                }
            }
        } ?: return
        try {
            routing.revert()
        } finally {
            monitor.withLock { active.remove(downstream, this@RoutingManager) }
        }
    }
}
