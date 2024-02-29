package be.mygod.vpnhotspot.net

import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.Process
import android.provider.Settings
import android.system.Os
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber

object VpnFirewallManager {
    /**
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-12.0.0_r1/framework/src/android/net/ConnectivitySettingsManager.java#378
     */
    @RequiresApi(31)
    private const val UIDS_ALLOWED_ON_RESTRICTED_NETWORKS = "uids_allowed_on_restricted_networks"

    @get:RequiresApi(29)
    val dumpCommand get() = if (Build.VERSION.SDK_INT >= 33) {
        "dumpsys ${Context.CONNECTIVITY_SERVICE} trafficcontroller"
    } else "dumpsys netd trafficcontroller"

    /**
     * This feature was introduced in Android 10:
     * https://android.googlesource.com/platform/frameworks/base/+/android-10.0.0_r47/services/core/java/com/android/server/connectivity/PermissionMonitor.java#241
     *
     * It was optional until Android 12 enforced the kernel upgrade: https://android-review.googlesource.com/c/platform/system/netd/+/1554261
     */
    private val bpfSupported by lazy {
        val properties by lazy { Class.forName("android.os.SystemProperties") }
        val firstApiIsHigh = { fallback: Long ->
            properties.getDeclaredMethod("getLong", String::class.java, Long::class.java)
                .invoke(null, "ro.product.first_api_level", fallback) as Long >= 28
        }
        when (Build.VERSION.SDK_INT) {
            28 -> false
            // https://android.googlesource.com/platform/system/bpf/+/android-10.0.0_r1/libbpf_android/BpfUtils.cpp#263
            29 -> firstApiIsHigh(29L) && Os.uname().release.split('.', limit = 3).let { version ->
                val major = version[0].toInt()
                major > 4 || major == 4 && version[1].toInt() >= 9
            }
            // https://android.googlesource.com/platform/system/bpf/+/android-11.0.0_r1/libbpf_android/BpfUtils.cpp#133
            30 -> {
                val kernel = "^(\\d+)\\.(\\d+)\\.(\\d+).*".toPattern().matcher(Os.uname().release).let { version ->
                    if (!version.matches()) return@let 0
                    version.group(1)!!.toInt() * 65536 + version.group(2)!!.toInt() * 256 + version.group(3)!!.toInt()
                }
                kernel >= 4 * 65536 + 14 * 256 ||
                        properties.getDeclaredMethod("getBoolean", String::class.java, Boolean::class.java)
                            .invoke(null, "ro.kernel.ebpf.supported", false) as Boolean ||
                        firstApiIsHigh(30L) && kernel >= 4 * 65536 + 9 * 256
            }
            else -> true
        }
    }

    /**
     * https://android.googlesource.com/platform/system/netd/+/android-12.1.0_r1/server/TrafficController.cpp#1003
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-13.0.0_r1/service/native/TrafficController.cpp#824
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-14.0.0_r1/service/src/com/android/server/BpfNetMaps.java#1130
     */
    private val firewallMatcher by lazy { "^\\s*${Process.myUid()}\\D* IIF_MATCH ".toRegex(RegexOption.MULTILINE) }

    fun setup(transaction: RootSession.Transaction) {
        if (!bpfSupported || app.checkSelfPermission("android.permission.CONNECTIVITY_USE_RESTRICTED_NETWORKS") ==
            PackageManager.PERMISSION_GRANTED) return
        if (Build.VERSION.SDK_INT < 31) {
            SmartSnackbar.make(R.string.warn_vpn_firewall).show()
            return
        }
        val uid = Process.myUid()
        val allowed = Settings.Global.getString(app.contentResolver, UIDS_ALLOWED_ON_RESTRICTED_NETWORKS)
            ?.splitToSequence(';')?.mapNotNull { it.toIntOrNull() }?.toMutableSet() ?: mutableSetOf()
        if (!allowed.contains(uid)) {
            allowed.add(uid)
            transaction.exec("settings put global $UIDS_ALLOWED_ON_RESTRICTED_NETWORKS '${allowed.joinToString(";")}'")
        }
        val result = transaction.execQuiet(dumpCommand)
        result.message(listOf(dumpCommand), false)?.let { msg ->
            return Timber.w(Exception(msg))
        }
        // firewall was enabled before changing exclusion rules
        if (firewallMatcher.containsMatchIn(result.out)) SmartSnackbar.make(R.string.error_vpn_firewall_reboot).show()
    }
}
