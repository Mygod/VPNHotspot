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
     * Shizuku mode builds a second, privileged [ConnectivityManager] without running any of its
     * constructors, because every one of them can assign the process-wide singleton. Every declared
     * instance field is assigned into the allocated copy from the ordinary manager; these two are the
     * ones subsequently overridden, and the only ones the copy owns rather than aliases.
     */
    val ConnectivityManager_mContext by lazy {
        init
        ConnectivityManager::class.java.getDeclaredField("mContext").apply { isAccessible = true }
    }
    val ConnectivityManager_mService by lazy {
        init
        ConnectivityManager::class.java.getDeclaredField("mService").apply { isAccessible = true }
    }
    /**
     * Only ever read, as the invariant that the singleton was not written.
     */
    val ConnectivityManager_sInstance by lazy {
        init
        ConnectivityManager::class.java.getDeclaredField("sInstance").apply { isAccessible = true }
    }
    /**
     * A lazily loaded per-instance feature cache, so a privileged copy holding a null cache would issue
     * a second wrapped transaction to fill it. Warming this on the ordinary manager before the field
     * copy leaves the copy with a populated value; the cache holds every feature in one word, so the
     * argument does not matter. Absent on releases whose Connectivity module has no such cache.
     *
     * Introduction and first use are different facts and both matter here. The method and its cache
     * field appear together in API 35, the method still `private` there and with no caller in the
     * module at all, which is why absence is only worth reporting from there; the call sites that
     * consult it - tagged `requestNetwork` and `NetworkAgent` construction - begin in API 36. A runtime
     * probe rather than an `SDK_INT` branch, because Mainline can backport either half. The first two
     * links are that API 35 method and its cache field, then the API 36 `NetworkAgent` use, then the
     * current shape.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-15.0.0_r1/framework/src/android/net/ConnectivityManager.java#4551
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-15.0.0_r1/framework/src/android/net/ConnectivityManager.java#1240
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
     * The exact `NetworkRequest` ConnectivityService returned for a request this app registered, which
     * `ConnectivityManager` records on the callback and nowhere else an app can reach.
     *
     * Shizuku mode needs it as a *retained* handle rather than as a lookup. `unregisterNetworkCallback` finds
     * the request through a process-static map, calls `releaseNetworkRequest`, and then - on any normal RPC
     * return, the return of a release the service authorized against a different UID included - removes that
     * mapping and marks the callback already-unregistered. So the one call that could reissue the release is
     * also the call that destroys the only thing it could reissue it with. Reading this field before any
     * release is attempted is what keeps the retry possible.
     *
     * Also the platform's own answer to "did registration take effect": it is assigned from the service's
     * return value inside the same monitor as the transaction, so a non-null value proves the request exists.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#4783
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#5604
     */
    val NetworkCallback_networkRequest by lazy {
        init
        ConnectivityManager.NetworkCallback::class.java.getDeclaredField("networkRequest").apply {
            isAccessible = true
        }
    }

    /**
     * A shape check for the direct release, and nothing else: resolving the member issues no transaction and
     * mutates nothing, so it can run before the session creates anything.
     *
     * The actual release goes through the typed stub, because that is what carries the exact
     * `NetworkRequest` argument. What a typed proxy cannot do is fail early - an interface the installed
     * Connectivity module never declared would only surface at call time, which for this call is after the
     * TUN, the request, the preference and the agent all exist and after the one release that could undo
     * them is the thing that does not work. Asking for the member up front turns that into a refusal to
     * start.
     *
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
     * `TestNetworkManager` has a single one-argument constructor, and its parameter type is the
     * TestNetwork interface under whichever name the installed Connectivity module uses: the type is
     * jarjar-relocated on some releases and not others, and a module update can move it without
     * moving [Build.VERSION.SDK_INT]. Deriving the name from the constructor instead of resolving a
     * candidate list makes relocation a non-issue.
     */
    val TestNetworkManager_constructor by lazy {
        init
        TestNetworkManager::class.java.declaredConstructors.single().apply { isAccessible = true }
    }
    val ITestNetworkManager_asInterface by lazy {
        init
        val iface = TestNetworkManager_constructor.parameterTypes.single()
        Class.forName("${iface.name}\$Stub", true, iface.classLoader)
            .getDeclaredMethod("asInterface", IBinder::class.java).apply { isAccessible = true }
    }

    /**
     * Wrapping the tethering connector needs its own `Stub`, unlike the live instance
     * [TetheringManager_getConnector] hands out. The name is derived from the interface this app
     * already casts that instance to, so it shares fate with that cast rather than adding a second
     * relocation assumption.
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
     * An availability check for [be.mygod.librootkotlinx.io.pid], which owns the accessor itself; this only
     * asks whether the owning class and field are there.
     *
     * Shizuku mode needs the answer *before* it launches anything. The launched child's pid is what SIGKILL
     * targets, and a child that never authenticates has no peer credentials to take it from, so a device
     * where this field is missing has no fence for an unresponsive child at all - which is a reason to
     * refuse the mode rather than to discover it during a stop. `java.lang.UNIXProcess` declares the same
     * private `int pid` on every supported release.
     *
     * https://android.googlesource.com/platform/libcore/+/refs/tags/android-13.0.0_r1/ojluni/src/main/java/java/lang/UNIXProcess.java#56
     * https://android.googlesource.com/platform/libcore/+/refs/tags/android-17.0.0_r1/ojluni/src/main/java/java/lang/UNIXProcess.java#56
     */
    val UNIXProcess_pid by lazy {
        init
        Class.forName("java.lang.UNIXProcess").getDeclaredField("pid")
    }
}
