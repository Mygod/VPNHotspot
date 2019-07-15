package be.mygod.vpnhotspot

import android.annotation.TargetApi
import android.os.Build
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.util.putIfAbsentCompat
import be.mygod.vpnhotspot.util.removeCompat
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber

abstract class RoutingManager(private val caller: Any, val downstream: String, private val isWifi: Boolean) {
    companion object {
        private const val KEY_MASQUERADE_MODE = "service.masqueradeMode"
        var masqueradeMode: Routing.MasqueradeMode
            @TargetApi(28) get() = app.pref.run {
                getString(KEY_MASQUERADE_MODE, null)?.let { return@run Routing.MasqueradeMode.valueOf(it) }
                if (getBoolean("service.masquerade", true)) // legacy settings
                    Routing.MasqueradeMode.Simple else Routing.MasqueradeMode.None
            }.let {
                // older app version enabled netd for everyone. should check again here
                if (Build.VERSION.SDK_INT >= 28 || it != Routing.MasqueradeMode.Netd) it
                else Routing.MasqueradeMode.Simple
            }
            set(value) = app.pref.edit().putString(KEY_MASQUERADE_MODE, value.name).apply()

        private val active = mutableMapOf<String, RoutingManager>()

        fun clean(reinit: Boolean = true) {
            for (manager in active.values) manager.routing?.stop()
            try {
                Routing.clean()
            } catch (e: RuntimeException) {
                Timber.d(e)
                SmartSnackbar.make(e).show()
                return
            }
            if (reinit) for (manager in active.values) manager.initRouting()
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
            commit(true)
        }
    }

    val started get() = active[downstream] === this
    private var routing: Routing? = null
    init {
        if (isWifi) WifiDoubleLock.acquire(this)
    }

    fun start() = when (active.putIfAbsentCompat(downstream, this)) {
        null -> initRouting()
        this -> true    // already started
        else -> throw IllegalStateException("Double routing detected from $caller")
    }

    private fun initRouting() = try {
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
        SmartSnackbar.make(e).show()
        Timber.w(e)
        routing = null
        false
    }

    protected abstract fun Routing.configure()

    fun stop() {
        if (active.removeCompat(downstream, this)) routing?.revert()
    }

    fun destroy() {
        if (isWifi) WifiDoubleLock.release(this)
        stop()
    }
}
