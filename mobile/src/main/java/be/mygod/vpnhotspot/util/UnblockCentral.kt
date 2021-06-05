package be.mygod.vpnhotspot.util

import android.annotation.SuppressLint
import android.net.wifi.SoftApConfiguration
import android.net.wifi.p2p.WifiP2pConfig
import android.os.Build
import androidx.annotation.RequiresApi
import java.lang.reflect.Method

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
    private val init by lazy {
        if (Build.VERSION.SDK_INT < 28) return@lazy
        // TODO: fix this not working when targeting API 30+
        val getDeclaredMethod = Class::class.java.getDeclaredMethod("getDeclaredMethod",
            String::class.java, arrayOf<Class<*>>()::class.java)
        val clazz = Class.forName("dalvik.system.VMRuntime")
        val setHiddenApiExemptions = getDeclaredMethod(clazz, "setHiddenApiExemptions",
            arrayOf(Array<String>::class.java)) as Method
        setHiddenApiExemptions(clazz.getDeclaredMethod("getRuntime")(null), arrayOf(""))
    }

    @RequiresApi(31)
    fun setUserConfiguration(clazz: Class<*>) = init.let {
        clazz.getDeclaredMethod("setUserConfiguration", Boolean::class.java)
    }

    @get:RequiresApi(31)
    val SoftApConfiguration_BAND_TYPES get() = init.let {
        SoftApConfiguration::class.java.getDeclaredField("BAND_TYPES").get(null) as IntArray
    }

    @RequiresApi(31)
    fun getApInstanceIdentifier(clazz: Class<*>) = init.let { clazz.getDeclaredMethod("getApInstanceIdentifier") }

    @get:RequiresApi(29)
    val WifiP2pConfig_Builder_mNetworkName get() = init.let {
        WifiP2pConfig.Builder::class.java.getDeclaredField("mNetworkName").apply { isAccessible = true }
    }
}
