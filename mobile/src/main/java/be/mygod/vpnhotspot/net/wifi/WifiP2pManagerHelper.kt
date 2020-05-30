package be.mygod.vpnhotspot.net.wifi

import android.annotation.SuppressLint
import android.net.wifi.WpsInfo
import android.net.wifi.p2p.WifiP2pGroup
import android.net.wifi.p2p.WifiP2pManager
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.callSuper
import timber.log.Timber
import java.lang.reflect.InvocationHandler
import java.lang.reflect.Method
import java.lang.reflect.Proxy

object WifiP2pManagerHelper {
    const val UNSUPPORTED = -2
    const val WIFI_P2P_PERSISTENT_GROUPS_CHANGED_ACTION = "android.net.wifi.p2p.PERSISTENT_GROUPS_CHANGED"

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
        try {
            setWifiP2pChannels.invoke(this, c, lc, oc, listener)
        } catch (_: NoSuchMethodException) {
            app.logEvent("NoSuchMethod_setWifiP2pChannels")
            listener.onFailure(UNSUPPORTED)
        }
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
        startWps!!.invoke(this, c, wps, listener)
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
        try {
            deletePersistentGroup.invoke(this, c, netId, listener)
        } catch (_: NoSuchMethodException) {
            app.logEvent("NoSuchMethod_deletePersistentGroup")
            listener.onFailure(UNSUPPORTED)
        }
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
                arrayOf(interfacePersistentGroupInfoListener), object : InvocationHandler {
            override fun invoke(proxy: Any, method: Method, args: Array<out Any?>?): Any? = when (method.name) {
                "onPersistentGroupInfoAvailable" -> {
                    if (args?.size != 1) Timber.w(IllegalArgumentException("Unexpected args: $args"))
                    @Suppress("UNCHECKED_CAST") listener(getGroupList.invoke(args!![0]) as Collection<WifiP2pGroup>)
                }
                else -> callSuper(interfacePersistentGroupInfoListener, proxy, method, args)
            }
        })
        requestPersistentGroupInfo.invoke(this, c, proxy)
    }
}
