package be.mygod.vpnhotspot.net

import android.provider.Settings
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.RootSession

/**
 * It's hard to change tethering rules with Tethering hardware acceleration enabled for now.
 *
 * See also:
 *   android.provider.Settings.Global.TETHER_OFFLOAD_DISABLED
 *   https://android.googlesource.com/platform/frameworks/base/+/android-8.1.0_r1/services/core/java/com/android/server/connectivity/tethering/OffloadHardwareInterface.java#45
 *   https://android.googlesource.com/platform/hardware/qcom/data/ipacfg-mgr/+/master/msm8998/ipacm/src/IPACM_OffloadManager.cpp
 */
@RequiresApi(27)
object TetherOffloadManager {
    private const val TETHER_OFFLOAD_DISABLED = "tether_offload_disabled"
    @JvmStatic
    var enabled: Boolean
        get() = Settings.Global.getInt(app.contentResolver, TETHER_OFFLOAD_DISABLED, 0) == 0
        set(value) {
            RootSession.use {
                it.exec("settings put global $TETHER_OFFLOAD_DISABLED ${if (value) 0 else 1}")
            }
        }
}
