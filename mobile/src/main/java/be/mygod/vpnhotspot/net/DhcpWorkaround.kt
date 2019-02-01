package be.mygod.vpnhotspot.net

import android.content.SharedPreferences
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber

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
    fun enable(enabled: Boolean) {
        val action = if (enabled) "add" else "del"
        try {
            RootSession.use { it.exec("ip rule $action iif lo uidrange 0-0 lookup local_network priority 11000") }
        } catch (e: RootSession.UnexpectedOutputException) {
            if (e.result.out.isEmpty() && (e.result.code == 2 || e.result.code == 254) && if (enabled) {
                        e.result.err.joinToString("\n") == "RTNETLINK answers: File exists"
                    } else {
                        e.result.err.joinToString("\n") == "RTNETLINK answers: No such file or directory"
                    }) return
            Timber.w(e)
            SmartSnackbar.make(e).show()
        } catch (e: Exception) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
        }
    }

    override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
        if (key == KEY_ENABLED) enable(shouldEnable)
    }
}
