package be.mygod.vpnhotspot.net

import android.annotation.SuppressLint
import android.net.ConnectivityManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import com.android.dx.stock.ProxyBuilder
import timber.log.Timber

/**
 * Heavily based on:
 * https://github.com/aegis1980/WifiHotSpot
 * https://android.googlesource.com/platform/frameworks/base.git/+/android-7.0.0_r1/core/java/android/net/ConnectivityManager.java
 */
object TetheringManager {
    /**
     * Callback for use with [.startTethering] to find out whether tethering succeeded.
     */
    interface OnStartTetheringCallback {
        /**
         * Called when tethering has been successfully started.
         */
        fun onTetheringStarted() { }

        /**
         * Called when starting tethering failed.
         */
        fun onTetheringFailed() { }
    }

    /**
     * This is a sticky broadcast since almost forever.
     *
     * https://android.googlesource.com/platform/frameworks/base.git/+/2a091d7aa0c174986387e5d56bf97a87fe075bdb%5E%21/services/java/com/android/server/connectivity/Tethering.java
     */
    const val ACTION_TETHER_STATE_CHANGED = "android.net.conn.TETHER_STATE_CHANGED"
    private const val EXTRA_ACTIVE_TETHER_LEGACY = "activeArray"
    /**
     * gives a String[] listing all the interfaces currently in local-only
     * mode (ie, has DHCPv4+IPv6-ULA support and no packet forwarding)
     */
    @RequiresApi(26)
    private const val EXTRA_ACTIVE_LOCAL_ONLY = "localOnlyArray"
    /**
     * gives a String[] listing all the interfaces currently tethered
     * (ie, has DHCPv4 support and packets potentially forwarded/NATed)
     */
    @RequiresApi(26)
    private const val EXTRA_ACTIVE_TETHER = "tetherArray"
    /**
     * gives a String[] listing all the interfaces we tried to tether and
     * failed.  Use {@link #getLastTetherError} to find the error code
     * for any interfaces listed here.
     */
    const val EXTRA_ERRORED_TETHER = "erroredArray"
    const val TETHER_ERROR_NO_ERROR           = 0
    const val TETHER_ERROR_UNKNOWN_IFACE      = 1
    const val TETHER_ERROR_SERVICE_UNAVAIL    = 2
    const val TETHER_ERROR_UNSUPPORTED        = 3
    const val TETHER_ERROR_UNAVAIL_IFACE      = 4
    const val TETHER_ERROR_MASTER_ERROR       = 5
    const val TETHER_ERROR_TETHER_IFACE_ERROR = 6
    const val TETHER_ERROR_UNTETHER_IFACE_ERROR = 7
    const val TETHER_ERROR_ENABLE_NAT_ERROR     = 8
    const val TETHER_ERROR_DISABLE_NAT_ERROR    = 9
    const val TETHER_ERROR_IFACE_CFG_ERROR      = 10
    const val TETHER_ERROR_PROVISION_FAILED     = 11

    const val TETHERING_WIFI = 0
    /**
     * Requires MANAGE_USB permission, unfortunately.
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/7ca5d3a/services/usb/java/com/android/server/usb/UsbService.java#389
     */
    const val TETHERING_USB = 1
    /**
     * Requires BLUETOOTH permission.
     */
    const val TETHERING_BLUETOOTH = 2

    private val classOnStartTetheringCallback by lazy @SuppressLint("PrivateApi") {
        Class.forName("android.net.ConnectivityManager\$OnStartTetheringCallback")
    }
    private val startTethering by lazy {
        ConnectivityManager::class.java.getDeclaredMethod("startTethering",
                Int::class.java, Boolean::class.java, classOnStartTetheringCallback, Handler::class.java)
    }
    private val stopTethering by lazy {
        ConnectivityManager::class.java.getDeclaredMethod("stopTethering", Int::class.java)
    }
    private val getLastTetherError by lazy {
        ConnectivityManager::class.java.getDeclaredMethod("getLastTetherError", String::class.java)
    }

    /**
     * Runs tether provisioning for the given type if needed and then starts tethering if
     * the check succeeds. If no carrier provisioning is required for tethering, tethering is
     * enabled immediately. If provisioning fails, tethering will not be enabled. It also
     * schedules tether provisioning re-checks if appropriate.
     *
     * @param type The type of tethering to start. Must be one of
     *         {@link ConnectivityManager.TETHERING_WIFI},
     *         {@link ConnectivityManager.TETHERING_USB}, or
     *         {@link ConnectivityManager.TETHERING_BLUETOOTH}.
     * @param showProvisioningUi a boolean indicating to show the provisioning app UI if there
     *         is one. This should be true the first time this function is called and also any time
     *         the user can see this UI. It gives users information from their carrier about the
     *         check failing and how they can sign up for tethering if possible.
     * @param callback an {@link OnStartTetheringCallback} which will be called to notify the caller
     *         of the result of trying to tether.
     * @param handler {@link Handler} to specify the thread upon which the callback will be invoked.
     */
    @RequiresApi(24)
    fun start(type: Int, showProvisioningUi: Boolean, callback: OnStartTetheringCallback, handler: Handler? = null) {
        val proxy = ProxyBuilder.forClass(classOnStartTetheringCallback)
                .dexCache(app.deviceStorage.cacheDir)
                .handler { proxy, method, args ->
                    if (args.isNotEmpty()) Timber.w("Unexpected args for ${method.name}: $args")
                    when (method.name) {
                        "onTetheringStarted" -> {
                            callback.onTetheringStarted()
                            null
                        }
                        "onTetheringFailed" -> {
                            callback.onTetheringFailed()
                            null
                        }
                        else -> {
                            Timber.w("Unexpected method, calling super: $method")
                            ProxyBuilder.callSuper(proxy, method, args)
                        }
                    }
                }
                .build()
        startTethering.invoke(app.connectivity, type, showProvisioningUi, proxy, handler)
    }

    /**
     * Stops tethering for the given type. Also cancels any provisioning rechecks for that type if
     * applicable.
     *
     * @param type The type of tethering to stop. Must be one of
     *         {@link ConnectivityManager.TETHERING_WIFI},
     *         {@link ConnectivityManager.TETHERING_USB}, or
     *         {@link ConnectivityManager.TETHERING_BLUETOOTH}.
     */
    @RequiresApi(24)
    fun stop(type: Int) {
        stopTethering.invoke(app.connectivity, type)
    }

    /**
     * Get a more detailed error code after a Tethering or Untethering
     * request asynchronously failed.
     *
     * @param iface The name of the interface of interest
     * @return error The error code of the last error tethering or untethering the named
     *               interface
     */
    fun getLastTetherError(iface: String): Int = getLastTetherError.invoke(app.connectivity, iface) as Int

    fun getTetheredIfaces(extras: Bundle) = extras.getStringArrayList(
            if (Build.VERSION.SDK_INT >= 26) EXTRA_ACTIVE_TETHER else EXTRA_ACTIVE_TETHER_LEGACY)!!
    fun getLocalOnlyTetheredIfaces(extras: Bundle) =
            if (Build.VERSION.SDK_INT >= 26) extras.getStringArrayList(EXTRA_ACTIVE_LOCAL_ONLY)!!
            else emptyList<String>()
}
