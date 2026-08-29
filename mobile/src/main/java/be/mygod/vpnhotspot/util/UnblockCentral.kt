package be.mygod.vpnhotspot.util

import android.annotation.SuppressLint
import android.net.ConnectivityManager
import android.net.IConnectivityManager
import android.net.ITetheringConnector
import android.net.MacAddress
import android.net.NetworkRequest
import android.net.TestNetworkManager
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
import android.os.Looper
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

    val WifiManager_mService by lazy {
        init
        WifiManager::class.java.getDeclaredField("mService").apply { isAccessible = true }
    }

    /**
     * Some Google Wi-Fi Mainline releases make this proxy static, so their constructor omits the
     * implicit outer [WifiManager] parameter. Probe the runtime shape because APEX updates are
     * independent of the platform SDK level.
     */
    val WifiManager_SoftApCallbackProxy: (Any, Int) -> IBinder by lazy {
        init
        val clazz = Class.forName("android.net.wifi.WifiManager\$SoftApCallbackProxy")
        try {
            val constructor = clazz.getDeclaredConstructor(Executor::class.java,
                `WifiManager$SoftApCallback`::class.java, Int::class.javaPrimitiveType)
            constructor.isAccessible = true;
            { callback, mode -> constructor.newInstance(InPlaceExecutor, callback, mode) as IBinder }
        } catch (staticMissing: NoSuchMethodException) {
            try {
                val constructor = clazz.getDeclaredConstructor(WifiManager::class.java, Executor::class.java,
                    `WifiManager$SoftApCallback`::class.java, Int::class.javaPrimitiveType)
                if (Build.VERSION.SDK_INT >= 38) Timber.w(staticMissing)
                constructor.isAccessible = true;
                { callback, mode ->
                    constructor.newInstance(Services.wifi, InPlaceExecutor, callback, mode) as IBinder
                }
            } catch (e: NoSuchMethodException) {
                if (Build.VERSION.SDK_INT >= 33) Timber.w(e)
                try {
                    val constructor = clazz.getDeclaredConstructor(WifiManager::class.java, Executor::class.java,
                        `WifiManager$SoftApCallback`::class.java)
                    constructor.isAccessible = true;
                    { callback, _ -> constructor.newInstance(Services.wifi, InPlaceExecutor, callback) as IBinder }
                } catch (e: NoSuchMethodException) {
                    if (Build.VERSION.SDK_INT >= 30) Timber.w(e)
                    val constructor = clazz.getDeclaredConstructor(WifiManager::class.java, Looper::class.java,
                        `WifiManager$SoftApCallback`::class.java)
                    constructor.isAccessible = true;
                    { callback, _ ->
                        constructor.newInstance(Services.wifi, Looper.getMainLooper(), callback) as IBinder
                    }
                }
            }
        }
    }

    /**
     * Overridden after a constructor-free [ConnectivityManager] copy; all other instance fields stay aliased.
     */
    val ConnectivityManager_mContext by lazy {
        init
        ConnectivityManager::class.java.getDeclaredField("mContext").apply { isAccessible = true }
    }
    /** Privileged binder override paired with [ConnectivityManager_mContext]. */
    val ConnectivityManager_mService by lazy {
        init
        ConnectivityManager::class.java.getDeclaredField("mService").apply { isAccessible = true }
    }
    /** Read only to verify constructor-free allocation did not replace the process singleton. */
    val ConnectivityManager_sInstance by lazy {
        init
        ConnectivityManager::class.java.getDeclaredField("sInstance").apply { isAccessible = true }
    }
    /**
     * Warms the ordinary manager's feature cache before copying it. The member appears in API 35 and is
     * consumed from API 36; a runtime probe handles independently updated Connectivity modules.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-15.0.0_r1/framework/src/android/net/ConnectivityManager.java#4551
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-16.0.0_r1/framework/src/android/net/NetworkAgent.java#624
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#4896
     */
    val ConnectivityManager_isFeatureEnabled by lazy {
        init
        try {
            ConnectivityManager::class.java.getDeclaredMethod("isFeatureEnabled",
                Long::class.javaPrimitiveType).apply { isAccessible = true }
        } catch (e: NoSuchMethodException) {
            if (Build.VERSION.SDK_INT >= 35) Timber.w(e)
            null
        }
    }

    /**
     * Exact service-returned request handle. It classifies issuance and survives until direct privileged
     * release; the public unregister wrapper discards the callback mapping on any normal RPC return.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/ConnectivityManager.java#4189
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/ConnectivityManager.java#4858
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#4783
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#5605
     */
    val NetworkCallback_networkRequest by lazy {
        init
        ConnectivityManager.NetworkCallback::class.java.getDeclaredField("networkRequest").apply {
            isAccessible = true
        }
    }

    /**
     * Preflight for the typed direct release, resolved before the session mutates anything.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/IConnectivityManager.aidl#169
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/IConnectivityManager.aidl#174
     */
    @get:RequiresApi(33)
    val IConnectivityManager_releaseNetworkRequest by lazy {
        init
        IConnectivityManager::class.java.getDeclaredMethod("releaseNetworkRequest",
            NetworkRequest::class.java)
    }

    /**
     * Its sole constructor supplies the possibly jarjar-relocated TestNetwork interface runtime name.
     */
    val TestNetworkManager_constructor by lazy {
        init
        TestNetworkManager::class.java.declaredConstructors.single().apply { isAccessible = true }
    }
    /** Resolves `$Stub.asInterface` from [TestNetworkManager_constructor]'s runtime parameter class. */
    val ITestNetworkManager_asInterface by lazy {
        init
        val iface = TestNetworkManager_constructor.parameterTypes.single()
        Class.forName("${iface.name}\$Stub", true, iface.classLoader)
            .getDeclaredMethod("asInterface", IBinder::class.java).apply { isAccessible = true }
    }

    /**
     * Derives the connector Stub from the already-resolved interface rather than assuming relocation.
     */
    @get:RequiresApi(33)
    val ITetheringConnector_asInterface by lazy {
        init
        Class.forName("${ITetheringConnector::class.java.name}\$Stub")
            .getDeclaredMethod("asInterface", IBinder::class.java).apply { isAccessible = true }
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

    /**
     * Preflights librootkotlinx's reflected child PID before launch; cleanup needs it for SIGKILL fencing.
     * https://android.googlesource.com/platform/libcore/+/refs/tags/android-13.0.0_r1/ojluni/src/main/java/java/lang/UNIXProcess.java#56
     * https://android.googlesource.com/platform/libcore/+/refs/tags/android-17.0.0_r1/ojluni/src/main/java/java/lang/UNIXProcess.java#56
     */
    val UNIXProcess_pid by lazy {
        init
        Class.forName("java.lang.UNIXProcess").getDeclaredField("pid")
    }
}
