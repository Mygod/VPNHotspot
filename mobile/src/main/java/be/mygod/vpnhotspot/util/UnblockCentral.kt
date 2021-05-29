package be.mygod.vpnhotspot.util

import android.annotation.SuppressLint
import android.net.wifi.SoftApConfiguration
import android.net.wifi.p2p.WifiP2pConfig
import androidx.annotation.RequiresApi
import timber.log.Timber

/**
 * The central object for accessing all the useful blocked APIs. Thanks Google!
 *
 * Lazy cannot be used directly as it will create inner classes.
 */
@SuppressLint("BlockedPrivateApi", "DiscouragedPrivateApi")
@Suppress("FunctionName")
object UnblockCentral {
    /**
     * Retrieve this property before doing dangerous shit.
     */
    @get:RequiresApi(28)
    private val init by lazy {
        try {
            Class.forName("dalvik.system.VMDebug").getDeclaredMethod("allowHiddenApiReflectionFrom", Class::class.java)
                .invoke(null, UnblockCentral::class.java)
            true
        } catch (e: ReflectiveOperationException) {
            Timber.w(e)
            false
        }
    }

    @get:RequiresApi(31)
    val SoftApConfiguration_BAND_TYPES get() = init.let {
        SoftApConfiguration::class.java.getField("BAND_TYPES").get(null) as IntArray
    }

    @RequiresApi(31)
    fun getApInstanceIdentifier(clazz: Class<*>) = init.let { clazz.getDeclaredMethod("getApInstanceIdentifier") }

    @get:RequiresApi(29)
    val WifiP2pConfig_Builder_mNetworkName get() = init.let {
        WifiP2pConfig.Builder::class.java.getDeclaredField("mNetworkName").apply { isAccessible = true }
    }
}
