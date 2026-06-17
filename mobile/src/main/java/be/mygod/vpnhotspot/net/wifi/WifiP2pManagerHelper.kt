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
    private val requestPersistentGroupInfo by lazy {
        WifiP2pManager::class.java.getDeclaredMethod("requestPersistentGroupInfo",
            WifiP2pManager.Channel::class.java, `WifiP2pManager$PersistentGroupInfoListener`::class.java)
    }
    /**
     * Request a list of all the persistent p2p groups stored in the system, so an already-present group can be
     * adopted when the app has nothing persisted yet. We only read this; we never delete persistent groups.
     * Supplicant mode may also ignore the returned list and use this as a framework-owned P2P setup pulse: AOSP
     * `needsActiveP2p` does not exempt `REQUEST_PERSISTENT_GROUP_INFO`, so the framework calls `setupInterface()`
     * before permission-gated handling can return an empty list.
     *
     * Requires one of NETWORK_SETTING, NETWORK_STACK, or READ_WIFI_CREDENTIAL permission since API 30 for the group
     * list contents.
     *
     * Sources:
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-16.0.0_r1/service/java/com/android/server/wifi/p2p/WifiP2pServiceImpl.java#2114
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-16.0.0_r1/service/java/com/android/server/wifi/p2p/WifiP2pServiceImpl.java#2358
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-17.0.0_r1/service/java/com/android/server/wifi/p2p/WifiP2pServiceImpl.java#2147
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-17.0.0_r1/service/java/com/android/server/wifi/p2p/WifiP2pServiceImpl.java#2391
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-17.0.0_r1/service/java/com/android/server/wifi/p2p/WifiP2pServiceImpl.java#3178
     */
    suspend fun WifiP2pManager.requestPersistentGroupInfo(c: WifiP2pManager.Channel) =
        suspendCancellableCoroutine { cont ->
            requestPersistentGroupInfo(this, c, object : `WifiP2pManager$PersistentGroupInfoListener` {
                override fun onPersistentGroupInfoAvailable(groups: WifiP2pGroupList) {
                    cont.resume(groups.groupList)
                }
            })
        }
    suspend fun WifiP2pManager.requestP2pState(c: WifiP2pManager.Channel) =
        suspendCancellableCoroutine { cont -> requestP2pState(c) { cont.resume(it) } }
}
