package be.mygod.vpnhotspot.util

import android.annotation.SuppressLint
import android.net.MacAddress
import android.net.TetheringManager
import android.net.wifi.SoftApCapability
import android.net.wifi.SoftApConfiguration
import android.net.wifi.SoftApInfo
import android.net.wifi.WifiClient
import android.net.wifi.WifiManager
import android.net.wifi.`WifiManager$SoftApCallback`
import android.net.wifi.p2p.WifiP2pConfig
import android.os.Build
import android.os.IBinder
import android.service.quicksettings.TileService
import androidx.annotation.RequiresApi
import org.lsposed.hiddenapibypass.HiddenApiBypass
import timber.log.Timber
import java.util.concurrent.Executor

/**
 * The central object for accessing all the useful blocked APIs. Thanks Google!
 *
 * Lazy cannot be used directly as it will create inner classes.
 */
@SuppressLint("BlockedPrivateApi", "DiscouragedPrivateApi", "SoonBlockedPrivateApi")
object UnblockCentral {
    var needInit = true
    /**
     * Retrieve this property before doing dangerous shit.
     */
    private val init by lazy { if (needInit) check(HiddenApiBypass.setHiddenApiExemptions("")) }

    @RequiresApi(33)
    fun setRandomizedMacAddress(clazz: Class<*>) = init.let {
        clazz.getDeclaredMethod("setRandomizedMacAddress", MacAddress::class.java)
    }

    @RequiresApi(31)
    fun getCountryCode(capability: SoftApCapability) = init.let { capability.countryCode }

    @RequiresApi(31)
    fun getApInstanceIdentifier(info: SoftApInfo) = init.let { info.apInstanceIdentifier }

    @RequiresApi(31)
    fun getApInstanceIdentifier(client: WifiClient) = init.let { client.apInstanceIdentifier }

    @get:RequiresApi(31)
    val SoftApConfiguration_BAND_TYPES get() = init.let {
        SoftApConfiguration::class.java.getDeclaredField("BAND_TYPES").get(null) as IntArray
    }

    val WifiP2pConfig_Builder_mNetworkName by lazy {
        init
        WifiP2pConfig.Builder::class.java.getDeclaredField("mNetworkName").apply { isAccessible = true }
    }

    val TileService_mToken by lazy {
        init
        TileService::class.java.getDeclaredField("mToken").apply { isAccessible = true }
    }

    @get:RequiresApi(31)
    val WifiManager_mService by lazy {
        init
        WifiManager::class.java.getDeclaredField("mService").apply { isAccessible = true }
    }
    @get:RequiresApi(31)
    val WifiManager_SoftApCallbackProxy: (`WifiManager$SoftApCallback`, Int) -> IBinder by lazy {
        init
        val clazz = Class.forName("android.net.wifi.WifiManager\$SoftApCallbackProxy")
        try {
            val constructor = clazz.getDeclaredConstructor(WifiManager::class.java, Executor::class.java,
                `WifiManager$SoftApCallback`::class.java, Int::class.javaPrimitiveType)
            constructor.isAccessible = true;
            { callback, mode -> constructor.newInstance(Services.wifi, InPlaceExecutor, callback, mode) as IBinder }
        } catch (e: NoSuchMethodException) {
            if (Build.VERSION.SDK_INT >= 33) Timber.w(e)
            val constructor = clazz.getDeclaredConstructor(WifiManager::class.java, Executor::class.java,
                `WifiManager$SoftApCallback`::class.java)
            constructor.isAccessible = true;
            { callback, _ -> constructor.newInstance(Services.wifi, InPlaceExecutor, callback) as IBinder }
        }
    }

    @get:RequiresApi(30)
    val TetheringManager_ConnectorConsumer by lazy { Class.forName("android.net.TetheringManager\$ConnectorConsumer") }
    @get:RequiresApi(30)
    val TetheringManager_getConnector by lazy {
        init
        TetheringManager::class.java.getDeclaredMethod("getConnector", TetheringManager_ConnectorConsumer).apply {
            isAccessible = true
        }
    }

    /**
     * For [be.mygod.librootkotlinx.io.awaitExit].
      */
    val openPidFd get() = if (Build.VERSION.SDK_INT >= 31) try {
        init
    } catch (e: Exception) {
        Timber.w(e)
    } else { }
}
