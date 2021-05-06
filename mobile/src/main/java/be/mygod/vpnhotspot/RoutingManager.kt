package be.mygod.vpnhotspot

import android.annotation.TargetApi
import android.os.Build
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.runBlocking
import timber.log.Timber

abstract class RoutingManager(private val caller: Any, val downstream: String, private val forceWifi: Boolean = false) {
    companion object {
        private const val KEY_MASQUERADE_MODE = "service.masqueradeMode"
        var masqueradeMode: Routing.MasqueradeMode
            @TargetApi(28) get() = app.pref.run {
                getString(KEY_MASQUERADE_MODE, null)?.let { return@run Routing.MasqueradeMode.valueOf(it) }
                if (getBoolean("service.masquerade", true)) {   // legacy settings
                    Routing.MasqueradeMode.Simple
                } else Routing.MasqueradeMode.None
            }.let {
                // older app version enabled netd for everyone. should check again here
                if (Build.VERSION.SDK_INT >= 28 || it != Routing.MasqueradeMode.Netd) it
                else Routing.MasqueradeMode.Simple
            }
            set(value) = app.pref.edit().putString(KEY_MASQUERADE_MODE, value.name).apply()

        /**
         * Thread safety: needs protection by companion object!
         */
        private val active = mutableMapOf<String, RoutingManager>()

        fun clean(reinit: Boolean = true) = synchronized(this) {
            if (!reinit && active.isEmpty()) return@synchronized
            for (manager in active.values) manager.routing?.stop()
            try {
                runBlocking { Routing.clean() }
            } catch (e: Exception) {
                Timber.d(e)
                SmartSnackbar.make(e).show()
                return
            }
            if (reinit) for (manager in active.values) manager.initRoutingLocked()
        }
    }

    /**
     * Both repeater and local-only hotspot are Wi-Fi based.
     */
    class LocalOnly(caller: Any, downstream: String) : RoutingManager(caller, downstream, true) {
        override fun Routing.configure() {
            ipForward() // local only interfaces need to enable ip_forward
            forward()
            masquerade(masqueradeMode)
            commit()
        }
    }

    var started = false
        private set
    /**
     * Thread safety: needs protection by companion object!
     */
    private var routing: Routing? = null
    private var isWifi = forceWifi || TetherType.ofInterface(downstream).isWifi

    fun start() = synchronized(RoutingManager) {
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
                initRoutingLocked()
            }
            this -> true    // already started
            else -> error("Double routing detected for $downstream from $caller != ${other.caller}")
        }
    }

    private fun initRoutingLocked() = try {
        routing = Routing(caller, downstream).apply {
            try {
                configure()
            } catch (e: Exception) {
                revert()
                throw e
            }
        }
        true
    } catch (e: Exception) {
        when (e) {
            is Routing.InterfaceNotFoundException -> Timber.i(e)
            !is CancellationException -> Timber.w(e)
        }
        SmartSnackbar.make(e).show()
        routing = null
        false
    }

    protected abstract fun Routing.configure()

    fun stop() = synchronized(RoutingManager) {
        started = false
        if (active.remove(downstream, this)) {
            if (!forceWifi && Build.VERSION.SDK_INT >= 30) TetherType.listener -= this
            if (isWifi) WifiDoubleLock.release(this)
            routing?.revert()
        }
    }
}
