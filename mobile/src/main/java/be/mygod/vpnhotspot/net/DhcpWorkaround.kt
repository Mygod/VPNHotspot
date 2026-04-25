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
 * Use priority 11000 so this runs before RULE_PRIORITY_SECURE_VPN, which is 12000 on API 29..30
 * and 13000 in the latest AOSP checked below.
 * It would be great if we can gain better understanding into why this is only needed on some of the devices but not
 * others.
 *
 * Sources:
 * https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/server/RouteController.cpp#59
 * https://android.googlesource.com/platform/system/netd/+/e11b8688b1f99292ade06f89f957c1f7e76ceae9/server/RouteController.h#37
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
