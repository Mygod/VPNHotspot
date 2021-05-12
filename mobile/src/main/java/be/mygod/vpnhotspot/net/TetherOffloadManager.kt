package be.mygod.vpnhotspot.net

import android.os.Build
import android.provider.Settings
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.root.SettingsGlobalPut
import timber.log.Timber

/**
 * It's hard to change tethering rules with Tethering hardware acceleration enabled for now.
 *
 * See also:
 *   android.provider.Settings.Global.TETHER_OFFLOAD_DISABLED
 *   https://android.googlesource.com/platform/frameworks/base/+/android-8.1.0_r1/services/core/java/com/android/server/connectivity/tethering/OffloadHardwareInterface.java#45
 *   https://android.googlesource.com/platform/hardware/qcom/data/ipacfg-mgr/+/master/msm8998/ipacm/src/IPACM_OffloadManager.cpp
 */
object TetherOffloadManager {
    val supported by lazy {
        Build.VERSION.SDK_INT >= 27 || try {
            Settings.Global::class.java.getDeclaredField("TETHER_OFFLOAD_DISABLED").get(null).let {
                require(it == TETHER_OFFLOAD_DISABLED) { "Unknown field $it" }
            }
            true
        } catch (_: NoSuchFieldException) {
            false
        } catch (e: Exception) {
            Timber.w(e)
            false
        }
    }
    private const val TETHER_OFFLOAD_DISABLED = "tether_offload_disabled"
    val enabled get() = Settings.Global.getInt(app.contentResolver, TETHER_OFFLOAD_DISABLED, 0) == 0
    suspend fun setEnabled(value: Boolean) = SettingsGlobalPut.int(TETHER_OFFLOAD_DISABLED, if (value) 0 else 1)
}
