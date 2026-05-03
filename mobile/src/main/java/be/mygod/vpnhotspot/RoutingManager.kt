package be.mygod.vpnhotspot

import android.os.Build
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Ipv6Mode
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import timber.log.Timber
import androidx.core.content.edit

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

        suspend fun clean(reinit: Boolean = true) = monitor.withLock {
            if (!reinit && active.isEmpty()) return@withLock
            for (manager in active.values) {
                manager.routing?.stop(withdrawCleanupPrefixes = true)
            }
            try {
                Routing.clean()
            } catch (e: Exception) {
                Timber.d(e)
                SmartSnackbar.make(e).show()
                return@withLock
            }
            if (reinit) for (manager in active.values) manager.initRoutingLocked()
        }
    }

    /**
     * Both repeater and local-only hotspot are Wi-Fi based.
     */
    class LocalOnly(caller: Any, downstream: String) : RoutingManager(caller, downstream, true) {
        override suspend fun Routing.configure() {
            ipForward() // local only interfaces need to enable ip_forward
            forward()
            masquerade(masqueradeMode)
            when (ipv6Mode) {
                Ipv6Mode.Block, Ipv6Mode.System -> { }
                Ipv6Mode.Nat -> ipv6Nat()
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

    suspend fun start(fromMonitor: Boolean = false) = monitor.withLock {
        started = true
        when (val other = active.putIfAbsent(downstream, this)) {
            null -> {
                if (isWifi) WifiDoubleLock.acquire(this)
                if (!forceWifi && Build.VERSION.SDK_INT >= 30) TetherType.listener[this] = {
                    val isWifiNow = TetherType.ofInterface(downstream).isWifi
                    if (isWifi != isWifiNow) {
                        if (isWifi) WifiDoubleLock.release(this) else WifiDoubleLock.acquire(this)
                        isWifi = isWifiNow
                    }
                }
                initRoutingLocked(fromMonitor)
            }
            this -> true    // already started
            else -> {
                val msg = "Double routing detected for $downstream from $caller != ${other.caller}"
                Timber.w(RuntimeException(msg))
                SmartSnackbar.make(msg).show()
                false
            }
        }
    }

    private suspend fun initRoutingLocked(fromMonitor: Boolean = false) = try {
        routing = Routing(caller, downstream).apply {
            transaction = RootSession.beginTransaction()
            try {
                configure()
                commit()
            } catch (e: Exception) {
                revert()
                throw e
            }
        }
        true
    } catch (e: Exception) {
        when (e) {
            is Routing.InterfaceNotFoundException -> if (!fromMonitor) Timber.d(e)
            !is CancellationException -> Timber.w(e)
        }
        if (e !is Routing.InterfaceNotFoundException || !fromMonitor) SmartSnackbar.make(e).show()
        routing = null
        false
    }

    protected abstract suspend fun Routing.configure()

    suspend fun stop() = monitor.withLock {
        started = false
        if (active.remove(downstream, this)) {
            if (!forceWifi && Build.VERSION.SDK_INT >= 30) TetherType.listener -= this
            if (isWifi) WifiDoubleLock.release(this)
            routing?.revert()
        }
    }
}
