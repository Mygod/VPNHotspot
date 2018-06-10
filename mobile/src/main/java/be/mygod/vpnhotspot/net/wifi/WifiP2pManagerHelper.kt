package be.mygod.vpnhotspot.net.wifi

import android.annotation.SuppressLint
import android.net.wifi.WpsInfo
import android.net.wifi.p2p.WifiP2pGroup
import android.net.wifi.p2p.WifiP2pManager
import android.util.Log
import com.android.dx.stock.ProxyBuilder
import com.crashlytics.android.Crashlytics
import java.lang.reflect.Proxy
import java.util.regex.Pattern

object WifiP2pManagerHelper {
    private const val TAG = "WifiP2pManagerHelper"

    /**
     * Matches the output of dumpsys wifip2p. This part is available since Android 4.2.
     *
     * Related sources:
     *   https://android.googlesource.com/platform/frameworks/base/+/f0afe4144d09aa9b980cffd444911ab118fa9cbe%5E%21/wifi/java/android/net/wifi/p2p/WifiP2pService.java
     *   https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/a8d5e40/service/java/com/android/server/wifi/p2p/WifiP2pServiceImpl.java#639
     *
     *   https://android.googlesource.com/platform/frameworks/base.git/+/android-5.0.0_r1/core/java/android/net/NetworkInfo.java#433
     *   https://android.googlesource.com/platform/frameworks/base.git/+/220871a/core/java/android/net/NetworkInfo.java#415
     */
    val patternNetworkInfo = "^mNetworkInfo .* (isA|a)vailable: (true|false)".toPattern(Pattern.MULTILINE)

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
    fun WifiP2pManager.setWifiP2pChannels(c: WifiP2pManager.Channel, lc: Int, oc: Int,
                                                  listener: WifiP2pManager.ActionListener) {
        setWifiP2pChannels.invoke(this, c, lc, oc, listener)
    }

    /**
     * Available since Android 4.3.
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/android-4.3_r0.9/wifi/java/android/net/wifi/p2p/WifiP2pManager.java#958
     */
    private val startWps by lazy {
        WifiP2pManager::class.java.getDeclaredMethod("startWps",
                WifiP2pManager.Channel::class.java, WpsInfo::class.java, WifiP2pManager.ActionListener::class.java)
    }
    fun WifiP2pManager.startWps(c: WifiP2pManager.Channel, wps: WpsInfo,
                                        listener: WifiP2pManager.ActionListener) {
        startWps.invoke(this, c, wps, listener)
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
    fun WifiP2pManager.deletePersistentGroup(c: WifiP2pManager.Channel, netId: Int,
                                                     listener: WifiP2pManager.ActionListener) {
        deletePersistentGroup.invoke(this, c, netId, listener)
    }

    private val interfacePersistentGroupInfoListener by lazy @SuppressLint("PrivateApi") {
        Class.forName("android.net.wifi.p2p.WifiP2pManager\$PersistentGroupInfoListener")
    }
    private val getGroupList by lazy @SuppressLint("PrivateApi") {
        Class.forName("android.net.wifi.p2p.WifiP2pGroupList").getDeclaredMethod("getGroupList")
    }
    private val requestPersistentGroupInfo by lazy {
        WifiP2pManager::class.java.getDeclaredMethod("requestPersistentGroupInfo",
                WifiP2pManager.Channel::class.java, interfacePersistentGroupInfoListener)
    }
    /**
     * Request a list of all the persistent p2p groups stored in system.
     *
     * @param c is the channel created at {@link #initialize}
     * @param listener for callback when persistent group info list is available. Can be null.
     */
    fun WifiP2pManager.requestPersistentGroupInfo(c: WifiP2pManager.Channel,
                                                  listener: (Collection<WifiP2pGroup>) -> Unit) {
        val proxy = Proxy.newProxyInstance(interfacePersistentGroupInfoListener.classLoader,
                arrayOf(interfacePersistentGroupInfoListener), { proxy, method, args ->
            if (method.name == "onPersistentGroupInfoAvailable") {
                if (args.size != 1) Crashlytics.log(Log.WARN, TAG, "Unexpected args: $args")
                listener(getGroupList.invoke(args[0]) as Collection<WifiP2pGroup>)
                null
            } else {
                Crashlytics.log(Log.WARN, TAG, "Unexpected method, calling super: $method")
                ProxyBuilder.callSuper(proxy, method, args)
            }
        })
        requestPersistentGroupInfo.invoke(this, c, proxy)
    }

    /**
     * Available since Android 4.2.
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/android-4.2_r1/wifi/java/android/net/wifi/p2p/WifiP2pGroup.java#253
     */
    private val getNetworkId by lazy { WifiP2pGroup::class.java.getDeclaredMethod("getNetworkId") }
    val WifiP2pGroup.netId get() = getNetworkId.invoke(this) as Int
}
