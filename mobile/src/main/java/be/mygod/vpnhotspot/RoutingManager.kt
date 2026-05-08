package be.mygod.vpnhotspot

import android.os.Build
import androidx.core.content.edit
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.Routing.Ipv6Mode
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import timber.log.Timber

abstract class RoutingManager(private val caller: Any, val downstream: String, private val forceWifi: Boolean = false) {
    companion object {
        private const val KEY_MASQUERADE_MODE = "service.masqueradeMode"
        private const val KEY_IPV6_MODE = "service.ipv6Mode"
        var masqueradeMode: Routing.MasqueradeMode
            get() = app.pref.run {
                getString(KEY_MASQUERADE_MODE, null)?.let { return@run Routing.MasqueradeMode.valueOf(it) }
                if (getBoolean("service.masquerade", true)) {   // legacy settings
                    Routing.MasqueradeMode.Simple
                } else Routing.MasqueradeMode.None
            }
            set(value) = app.pref.edit { putString(KEY_MASQUERADE_MODE, value.name) }
        var ipv6Mode: Ipv6Mode
            get() = app.pref.run {
                getString(KEY_IPV6_MODE, null)?.let { return@run Ipv6Mode.valueOf(it) }
                if (getBoolean("service.disableIpv6", true)) Ipv6Mode.Block else Ipv6Mode.System
            }
            set(value) = app.pref.edit { putString(KEY_IPV6_MODE, value.name) }

        /**
         * Thread safety: needs protection by [monitor]!
         */
        private val active = mutableMapOf<String, RoutingManager>()
        private val monitor = Mutex()
        private var cleaning: CompletableDeferred<Unit>? = null

        suspend fun clean(reinit: Boolean = true) {
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
                    if (!reinit && active.isEmpty()) null else active.values.mapNotNull { manager ->
                        manager.routing.also {
                            manager.routing = null
                        }
                    }
                } ?: return
                for (routing in routings) routing.stopForClean()
                try {
                    Routing.clean()
                } catch (e: Exception) {
                    Timber.d(e)
                    SmartSnackbar.make(e).show()
                    return
                }
                val restarts = if (reinit) monitor.withLock {
                    active.values.mapNotNull { manager ->
                        if (!manager.started || manager.routing != null) null else {
                            val routing = Routing(manager.caller, manager.downstream)
                            manager.routing = routing
                            manager to routing
                        }
                    }
                } else emptyList()
                monitor.withLock {
                    if (cleaning === clean) cleaning = null
                }
                clean.complete(Unit)
                for ((manager, routing) in restarts) monitor.withLock {
                    if (manager.routing === routing) manager.startRoutingLocked(routing)
                }
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

    suspend fun start(): Boolean {
        while (true) {
            var clean: CompletableDeferred<Unit>? = null
            val startedNow = monitor.withLock {
                val currentClean = cleaning
                if (currentClean != null) {
                    clean = currentClean
                    null
                } else {
                    val routing = when (val other = active.putIfAbsent(downstream, this@RoutingManager)) {
                        null -> {
                            started = true
                            acquireLocked()
                            Routing(caller, downstream).also { this@RoutingManager.routing = it }
                        }
                        this@RoutingManager -> if (!started) null else if (this@RoutingManager.routing == null) {
                            Routing(caller, downstream).also { this@RoutingManager.routing = it }
                        } else return true
                        else -> {
                            val msg = "Double routing detected for $downstream from $caller != ${other.caller}"
                            Timber.w(RuntimeException(msg))
                            SmartSnackbar.make(msg).show()
                            null
                        }
                    }
                    routing?.let { startRoutingLocked(it) } ?: false
                }
            }
            (clean ?: return startedNow ?: false).await()
        }
    }

    private fun acquireLocked() {
        if (isWifi) WifiDoubleLock.acquire(this)
        if (!forceWifi && Build.VERSION.SDK_INT >= 30) TetherType.listener[this@RoutingManager] = {
            val isWifiNow = TetherType.ofInterface(downstream).isWifi
            if (isWifi != isWifiNow) {
                if (isWifi) WifiDoubleLock.release(this) else WifiDoubleLock.acquire(this)
                isWifi = isWifiNow
            }
        }
    }

    private fun startRoutingLocked(routing: Routing): Boolean {
        routing.masqueradeMode = masqueradeMode
        routing.configure()
        routing.start()
        return true
    }

    private fun forgetRoutingLocked(routing: Routing) {
        if (this.routing !== routing) return
        this.routing = null
        if (started) releaseLocked()
        started = false
        active.remove(downstream, this@RoutingManager)
    }

    private fun releaseLocked() {
        if (!forceWifi && Build.VERSION.SDK_INT >= 30) TetherType.listener -= this
        if (isWifi) WifiDoubleLock.release(this)
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
