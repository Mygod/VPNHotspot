package be.mygod.vpnhotspot.net

import android.content.SharedPreferences
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing.Companion.IP
import be.mygod.vpnhotspot.root.RoutingCommands
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber
import java.io.IOException

/**
 * Assuming RULE_PRIORITY_VPN_OUTPUT_TO_LOCAL = 11000.
 * Normally this is used to forward packets from remote to local, but it works anyways.
 * It just needs to be before RULE_PRIORITY_SECURE_VPN = 12000.
 * It would be great if we can gain better understanding into why this is only needed on some of the devices but not
 * others.
 *
 * Source: https://android.googlesource.com/platform/system/netd/+/b9baf26/server/RouteController.cpp#57
 */
object DhcpWorkaround : SharedPreferences.OnSharedPreferenceChangeListener {
    private const val KEY_ENABLED = "service.dhcpWorkaround"

    init {
        app.pref.registerOnSharedPreferenceChangeListener(this)
    }

    val shouldEnable get() = app.pref.getBoolean(KEY_ENABLED, false)
    fun enable(enabled: Boolean) = GlobalScope.launch {
        val action = if (enabled) "add" else "del"
        try {
            RootSession.use {
                try {
                    // ROUTE_TABLE_LOCAL_NETWORK: https://cs.android.com/android/platform/superproject/+/master:system/netd/server/RouteController.cpp;l=74;drc=b6dc40ac3d566d952d8445fc6ac796109c0cbc87
                    it.exec("$IP rule $action iif lo uidrange 0-0 lookup 97 priority 11000")
                } catch (e: RoutingCommands.UnexpectedOutputException) {
                    if (Routing.shouldSuppressIpError(e, enabled)) return@use
                    Timber.w(IOException("Failed to tweak dhcp workaround rule", e))
                    SmartSnackbar.make(e).show()
                }
            }
        } catch (_: CancellationException) {
        } catch (e: Exception) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
        }
    }

    override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
        if (key == KEY_ENABLED) enable(shouldEnable)
    }
}
