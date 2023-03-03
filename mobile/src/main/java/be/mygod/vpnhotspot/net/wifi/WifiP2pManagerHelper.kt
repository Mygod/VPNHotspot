package be.mygod.vpnhotspot.net.wifi

import android.annotation.SuppressLint
import android.net.MacAddress
import android.net.wifi.ScanResult
import android.net.wifi.WpsInfo
import android.net.wifi.p2p.WifiP2pGroup
import android.net.wifi.p2p.WifiP2pInfo
import android.net.wifi.p2p.WifiP2pManager
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.util.callSuper
import be.mygod.vpnhotspot.util.matches
import kotlinx.coroutines.CompletableDeferred
import java.lang.reflect.InvocationHandler
import java.lang.reflect.Method
import java.lang.reflect.Proxy

object WifiP2pManagerHelper {
    private class ResultListener : WifiP2pManager.ActionListener {
        val future = CompletableDeferred<Int?>()

        override fun onSuccess() {
            future.complete(null)
        }

        override fun onFailure(reason: Int) {
            future.complete(reason)
        }
    }

    const val UNSUPPORTED = -2

    /**
     * Available since Android 4.4.
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/android-4.4_r1/wifi/java/android/net/wifi/p2p/WifiP2pManager.java#994
     * Implementation: https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/d72d2f4/service/java/com/android/server/wifi/p2p/SupplicantP2pIfaceHal.java#1159
     */
    private val setWifiP2pChannels by lazy {
        WifiP2pManager::class.java.getDeclaredMethod("setWifiP2pChannels", WifiP2pManager.Channel::class.java,
                Int::class.java, Int::class.java, WifiP2pManager.ActionListener::class.java)
    }
    /**
     * Requires one of NETWORK_SETTING, NETWORK_STACK, or OVERRIDE_WIFI_CONFIG permission since API 30.
     */
    suspend fun WifiP2pManager.setWifiP2pChannels(c: WifiP2pManager.Channel, lc: Int, oc: Int): Int? {
        val result = ResultListener()
        try {
            setWifiP2pChannels(this, c, lc, oc, result)
        } catch (_: NoSuchMethodException) {
            app.logEvent("NoSuchMethod_setWifiP2pChannels")
            return UNSUPPORTED
        }
        return result.future.await()
    }

    @SuppressLint("MissingPermission")  // this method will fail correctly if permission is missing
    @RequiresApi(33)
    suspend fun WifiP2pManager.setVendorElements(c: WifiP2pManager.Channel,
                                                 ve: List<ScanResult.InformationElement>): Int? {
        val result = ResultListener()
        setVendorElements(c, ve, result)
        return result.future.await()
    }

    /**
     * Available since Android 4.3.
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/android-4.3_r0.9/wifi/java/android/net/wifi/p2p/WifiP2pManager.java#958
     */
    @JvmStatic
    val startWps by lazy {
        try {
            WifiP2pManager::class.java.getDeclaredMethod("startWps",
                    WifiP2pManager.Channel::class.java, WpsInfo::class.java, WifiP2pManager.ActionListener::class.java)
        } catch (_: NoSuchMethodException) {
            app.logEvent("NoSuchMethod_startWps")
            null
        }
    }
    fun WifiP2pManager.startWps(c: WifiP2pManager.Channel, wps: WpsInfo, listener: WifiP2pManager.ActionListener) {
        startWps!!(this, c, wps, listener)
    }

    /**
     * Available since Android 4.2.
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/android-4.2_r1/wifi/java/android/net/wifi/p2p/WifiP2pManager.java#1353
     */
    private val deletePersistentGroup by lazy {
        WifiP2pManager::class.java.getDeclaredMethod("deletePersistentGroup",
                WifiP2pManager.Channel::class.java, Int::class.java, WifiP2pManager.ActionListener::class.java)
    }
    /**
     * Requires one of NETWORK_SETTING, NETWORK_STACK, or READ_WIFI_CREDENTIAL permission since API 30.
     */
    suspend fun WifiP2pManager.deletePersistentGroup(c: WifiP2pManager.Channel, netId: Int): Int? {
        val result = ResultListener()
        try {
            deletePersistentGroup(this, c, netId, result)
        } catch (_: NoSuchMethodException) {
            app.logEvent("NoSuchMethod_deletePersistentGroup")
            return UNSUPPORTED
        }
        return result.future.await()
    }

    private val interfacePersistentGroupInfoListener by lazy {
        Class.forName("android.net.wifi.p2p.WifiP2pManager\$PersistentGroupInfoListener")
    }
    private val classWifiP2pGroupList by lazy { Class.forName("android.net.wifi.p2p.WifiP2pGroupList") }
    private val getGroupList by lazy { classWifiP2pGroupList.getDeclaredMethod("getGroupList") }
    private val requestPersistentGroupInfo by lazy {
        WifiP2pManager::class.java.getDeclaredMethod("requestPersistentGroupInfo",
                WifiP2pManager.Channel::class.java, interfacePersistentGroupInfoListener)
    }
    /**
     * Request a list of all the persistent p2p groups stored in system.
     *
     * Requires one of NETWORK_SETTING, NETWORK_STACK, or READ_WIFI_CREDENTIAL permission since API 30.
     *
     * @param c is the channel created at {@link #initialize}
     */
    suspend fun WifiP2pManager.requestPersistentGroupInfo(c: WifiP2pManager.Channel): Collection<WifiP2pGroup> {
        val result = CompletableDeferred<Collection<WifiP2pGroup>>()
        requestPersistentGroupInfo(this, c, Proxy.newProxyInstance(interfacePersistentGroupInfoListener.classLoader,
                arrayOf(interfacePersistentGroupInfoListener), object : InvocationHandler {
            override fun invoke(proxy: Any, method: Method, args: Array<out Any?>?): Any? = when {
                method.matches("onPersistentGroupInfoAvailable", classWifiP2pGroupList) -> {
                    @Suppress("UNCHECKED_CAST")
                    result.complete(getGroupList(args!![0]) as Collection<WifiP2pGroup>)
                }
                else -> callSuper(interfacePersistentGroupInfoListener, proxy, method, args)
            }
        }))
        return result.await()
    }

    suspend fun WifiP2pManager.requestConnectionInfo(c: WifiP2pManager.Channel) =
        CompletableDeferred<WifiP2pInfo?>().apply { requestConnectionInfo(c) { complete(it) } }.await()
    @SuppressLint("MissingPermission")  // missing permission simply leads to null result
    @RequiresApi(29)
    suspend fun WifiP2pManager.requestDeviceAddress(c: WifiP2pManager.Channel): MacAddress? {
        val future = CompletableDeferred<String?>()
        requestDeviceInfo(c) { future.complete(it?.deviceAddress) }
        return future.await()?.let {
            val address = if (it.isEmpty()) null else MacAddress.fromString(it)
            if (address == MacAddressCompat.ANY_ADDRESS) null else address
        }
    }
    @SuppressLint("MissingPermission")  // missing permission simply leads to null result
    suspend fun WifiP2pManager.requestGroupInfo(c: WifiP2pManager.Channel) =
        CompletableDeferred<WifiP2pGroup?>().apply { requestGroupInfo(c) { complete(it) } }.await()
    @RequiresApi(29)
    suspend fun WifiP2pManager.requestP2pState(c: WifiP2pManager.Channel) =
        CompletableDeferred<Int>().apply { requestP2pState(c) { complete(it) } }.await()
}
