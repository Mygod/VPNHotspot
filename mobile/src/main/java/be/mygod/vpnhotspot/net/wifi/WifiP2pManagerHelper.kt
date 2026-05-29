package be.mygod.vpnhotspot.net.wifi

import android.annotation.SuppressLint
import android.net.MacAddress
import android.net.wifi.ScanResult
import android.net.wifi.WpsInfo
import android.net.wifi.p2p.WifiP2pConfig
import android.net.wifi.p2p.WifiP2pGroupList
import android.net.wifi.p2p.WifiP2pManager
import android.net.wifi.p2p.`WifiP2pManager$PersistentGroupInfoListener`
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.MacAddressCompat
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlin.coroutines.resume

object WifiP2pManagerHelper {
    /**
     * Bridge a [WifiP2pManager.ActionListener]-based call into a suspend function: returns null on
     * success or the `WifiP2pManager` failure reason. The continuation is released once the platform
     * invokes the listener, so nothing heavier than it is retained behind the callback.
     */
    private suspend fun actionListener(block: (WifiP2pManager.ActionListener) -> Unit): Int? =
        suspendCancellableCoroutine { cont ->
            block(object : WifiP2pManager.ActionListener {
                override fun onSuccess() = cont.resume(null)
                override fun onFailure(reason: Int) = cont.resume(reason)
            })
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
    suspend fun WifiP2pManager.setWifiP2pChannels(c: WifiP2pManager.Channel, lc: Int, oc: Int): Int? = try {
        actionListener { setWifiP2pChannels(this, c, lc, oc, it) }
    } catch (_: NoSuchMethodException) {
        app.logEvent("NoSuchMethod_setWifiP2pChannels")
        UNSUPPORTED
    }

    @SuppressLint("MissingPermission")  // this method will fail correctly if permission is missing
    @RequiresApi(33)
    suspend fun WifiP2pManager.setVendorElements(c: WifiP2pManager.Channel,
                                                 ve: List<ScanResult.InformationElement>): Int? =
        actionListener { setVendorElements(c, ve, it) }

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
    suspend fun WifiP2pManager.startWps(c: WifiP2pManager.Channel, wps: WpsInfo): Int? =
        actionListener { startWps!!(this, c, wps, it) }

    /**
     * Create/remove a P2P group. Returns null on success or the `WifiP2pManager` failure reason.
     * A missing permission simply surfaces as a failure reason rather than throwing.
     */
    @SuppressLint("MissingPermission")
    suspend fun WifiP2pManager.createGroup(c: WifiP2pManager.Channel): Int? =
        actionListener { createGroup(c, it) }
    @SuppressLint("MissingPermission")
    suspend fun WifiP2pManager.createGroup(c: WifiP2pManager.Channel, config: WifiP2pConfig): Int? =
        actionListener { createGroup(c, config, it) }
    @SuppressLint("MissingPermission")
    suspend fun WifiP2pManager.removeGroup(c: WifiP2pManager.Channel): Int? =
        actionListener { removeGroup(c, it) }

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
    suspend fun WifiP2pManager.deletePersistentGroup(c: WifiP2pManager.Channel, netId: Int): Int? = try {
        actionListener { deletePersistentGroup(this, c, netId, it) }
    } catch (_: NoSuchMethodException) {
        app.logEvent("NoSuchMethod_deletePersistentGroup")
        UNSUPPORTED
    }

    private val requestPersistentGroupInfo by lazy {
        WifiP2pManager::class.java.getDeclaredMethod("requestPersistentGroupInfo",
            WifiP2pManager.Channel::class.java, `WifiP2pManager$PersistentGroupInfoListener`::class.java)
    }
    /**
     * Request a list of all the persistent p2p groups stored in system.
     *
     * Requires one of NETWORK_SETTING, NETWORK_STACK, or READ_WIFI_CREDENTIAL permission since API 30.
     *
     * @param c is the channel created at {@link #initialize}
     */
    suspend fun WifiP2pManager.requestPersistentGroupInfo(c: WifiP2pManager.Channel) =
        suspendCancellableCoroutine { cont ->
            requestPersistentGroupInfo(this, c, object : `WifiP2pManager$PersistentGroupInfoListener` {
                override fun onPersistentGroupInfoAvailable(groups: WifiP2pGroupList) {
                    cont.resume(groups.groupList)
                }
            })
        }

    suspend fun WifiP2pManager.requestConnectionInfo(c: WifiP2pManager.Channel) =
        suspendCancellableCoroutine { cont -> requestConnectionInfo(c) { cont.resume(it) } }
    @SuppressLint("MissingPermission")  // missing permission simply leads to null result
    suspend fun WifiP2pManager.requestDeviceAddress(c: WifiP2pManager.Channel) = suspendCancellableCoroutine { cont ->
        requestDeviceInfo(c) { cont.resume(it?.deviceAddress) }
    }?.let {
        val address = if (it.isEmpty()) null else MacAddress.fromString(it)
        if (address == MacAddressCompat.ANY_ADDRESS) null else address
    }
    @SuppressLint("MissingPermission")  // missing permission simply leads to null result
    suspend fun WifiP2pManager.requestGroupInfo(c: WifiP2pManager.Channel) =
        suspendCancellableCoroutine { cont -> requestGroupInfo(c) { cont.resume(it) } }
    suspend fun WifiP2pManager.requestP2pState(c: WifiP2pManager.Channel) =
        suspendCancellableCoroutine { cont -> requestP2pState(c) { cont.resume(it) } }
}
