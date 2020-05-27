package be.mygod.vpnhotspot.net

import android.content.Intent
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.LinkAddress
import android.os.Build
import android.os.Handler
import androidx.annotation.RequiresApi
import androidx.collection.SparseArrayCompat
import androidx.core.os.BuildCompat
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import com.android.dx.stock.ProxyBuilder
import timber.log.Timber
import java.lang.ref.WeakReference
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Proxy
import java.util.concurrent.Executor

/**
 * Heavily based on:
 * https://github.com/aegis1980/WifiHotSpot
 * https://android.googlesource.com/platform/frameworks/base.git/+/android-7.0.0_r1/core/java/android/net/ConnectivityManager.java
 */
object TetheringManager {
    /**
     * Callback for use with [startTethering] to find out whether tethering succeeded.
     */
    interface StartTetheringCallback {
        /**
         * Called when tethering has been successfully started.
         */
        fun onTetheringStarted() { }

        /**
         * Called when starting tethering failed.
         *
         * @param error The error that caused the failure.
         */
        fun onTetheringFailed(error: Int? = null) { }

        fun onException() { }
    }

    /**
     * Use with {@link #getSystemService(String)} to retrieve a {@link android.net.TetheringManager}
     * for managing tethering functions.
     * @hide
     * @see android.net.TetheringManager
     */
    const val TETHERING_SERVICE = "tethering"

    /**
     * This is a sticky broadcast since almost forever.
     *
     * https://android.googlesource.com/platform/frameworks/base.git/+/2a091d7aa0c174986387e5d56bf97a87fe075bdb%5E%21/services/java/com/android/server/connectivity/Tethering.java
     */
    const val ACTION_TETHER_STATE_CHANGED = "android.net.conn.TETHER_STATE_CHANGED"
    @RequiresApi(26)
    private const val EXTRA_ACTIVE_LOCAL_ONLY_LEGACY = "localOnlyArray"
    private const val EXTRA_ACTIVE_TETHER_LEGACY = "activeArray"
    /**
     * gives a String[] listing all the interfaces currently in local-only
     * mode (ie, has DHCPv4+IPv6-ULA support and no packet forwarding)
     */
    @RequiresApi(30)
    private const val EXTRA_ACTIVE_LOCAL_ONLY = "android.net.extra.ACTIVE_LOCAL_ONLY"
    /**
     * gives a String[] listing all the interfaces currently tethered
     * (ie, has DHCPv4 support and packets potentially forwarded/NATed)
     */
    @RequiresApi(26)
    private const val EXTRA_ACTIVE_TETHER = "tetherArray"
    /**
     * gives a String[] listing all the interfaces we tried to tether and
     * failed.  Use [getLastTetherError] to find the error code
     * for any interfaces listed here.
     */
    const val EXTRA_ERRORED_TETHER = "erroredArray"

    /**
     * Wifi tethering type.
     * @see [startTethering].
     */
    @RequiresApi(24)
    const val TETHERING_WIFI = 0
    /**
     * USB tethering type.
     *
     * Requires MANAGE_USB permission, unfortunately.
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/7ca5d3a/services/usb/java/com/android/server/usb/UsbService.java#389
     * @see [startTethering].
     */
    @RequiresApi(24)
    const val TETHERING_USB = 1
    /**
     * Bluetooth tethering type.
     *
     * Requires BLUETOOTH permission, or BLUETOOTH_PRIVILEGED on API 30+.
     * @see [startTethering].
     */
    @RequiresApi(24)
    const val TETHERING_BLUETOOTH = 2

    @get:RequiresApi(30)
    private val clazz by lazy { Class.forName("android.net.TetheringManager") }
    @get:RequiresApi(30)
    private val instance by lazy { app.getSystemService(TETHERING_SERVICE) }

    @get:RequiresApi(24)
    private val classOnStartTetheringCallback by lazy {
        Class.forName("android.net.ConnectivityManager\$OnStartTetheringCallback")
    }
    @get:RequiresApi(24)
    private val startTetheringLegacy by lazy {
        ConnectivityManager::class.java.getDeclaredMethod("startTethering",
                Int::class.java, Boolean::class.java, classOnStartTetheringCallback, Handler::class.java)
    }
    @get:RequiresApi(24)
    private val stopTetheringLegacy by lazy {
        ConnectivityManager::class.java.getDeclaredMethod("stopTethering", Int::class.java)
    }
    private val getLastTetherError by lazy {
        ConnectivityManager::class.java.getDeclaredMethod("getLastTetherError", String::class.java)
    }

    @get:RequiresApi(30)
    private val classTetheringRequestBuilder by lazy {
        Class.forName("android.net.TetheringManager\$TetheringRequest\$Builder")
    }
    @get:RequiresApi(30)
    private val newTetheringRequestBuilder by lazy { classTetheringRequestBuilder.getConstructor(Int::class.java) }
    @get:RequiresApi(30)
    private val setStaticIpv4Addresses by lazy {
        classTetheringRequestBuilder.getDeclaredMethod("setStaticIpv4Addresses",
                LinkAddress::class.java, LinkAddress::class.java)
    }
    @get:RequiresApi(30)
    private val setExemptFromEntitlementCheck by lazy {
        classTetheringRequestBuilder.getDeclaredMethod("setExemptFromEntitlementCheck", Boolean::class.java)
    }
    @get:RequiresApi(30)
    private val setShouldShowEntitlementUi by lazy {
        classTetheringRequestBuilder.getDeclaredMethod("setShouldShowEntitlementUi", Boolean::class.java)
    }
    @get:RequiresApi(30)
    private val build by lazy { classTetheringRequestBuilder.getDeclaredMethod("build") }

    @get:RequiresApi(30)
    private val interfaceStartTetheringCallback by lazy {
        Class.forName("android.net.TetheringManager\$StartTetheringCallback")
    }
    @get:RequiresApi(30)
    private val startTethering by lazy {
        clazz.getDeclaredMethod("startTethering", Class.forName("android.net.TetheringManager\$TetheringRequest"),
                Executor::class.java, interfaceStartTetheringCallback)
    }
    @get:RequiresApi(30)
    private val stopTethering by lazy { clazz.getDeclaredMethod("stopTethering", Int::class.java) }

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
     * @param handler {@link Handler} to specify the thread upon which the callback will be invoked
     * @param address A pair (localIPv4Address, clientAddress) for API 30+. If present, it
     *         configures tethering with static IPv4 assignment.
     *
     *         A DHCP server will be started, but will only be able to offer the client address.
     *         The two addresses must be in the same prefix.
     *
     *         localIPv4Address: The preferred local IPv4 link address to use.
     *         clientAddress: The static client address.
     * @see setStaticIpv4Addresses
     */
    @RequiresApi(24)
    fun startTethering(type: Int, showProvisioningUi: Boolean, callback: StartTetheringCallback,
                       handler: Handler? = null, address: Pair<LinkAddress, LinkAddress>? = null) {
        val reference = WeakReference(callback)
        if (BuildCompat.isAtLeastR()) try {
            val request = newTetheringRequestBuilder.newInstance(type).let { builder ->
                // setting exemption requires TETHER_PRIVILEGED permission
                if (app.checkSelfPermission("android.permission.TETHER_PRIVILEGED") ==
                        PackageManager.PERMISSION_GRANTED) setExemptFromEntitlementCheck.invoke(builder, true)
                setShouldShowEntitlementUi.invoke(builder, showProvisioningUi)
                if (address != null) {
                    val (localIPv4Address, clientAddress) = address
                    setStaticIpv4Addresses(builder, localIPv4Address, clientAddress)
                }
                build.invoke(this)
            }
            val executor = Executor { if (handler == null) it.run() else handler.post(it) }
            val proxy = Proxy.newProxyInstance(interfaceStartTetheringCallback.classLoader,
                    arrayOf(interfaceStartTetheringCallback)) { proxy, method, args ->
                @Suppress("NAME_SHADOWING") val callback = reference.get()
                when (val name = method.name) {
                    "onTetheringStarted" -> {
                        if (args.isNotEmpty()) Timber.w("Unexpected args for $name: $args")
                        callback?.onTetheringStarted()
                        null
                    }
                    "onTetheringFailed" -> {
                        if (args.size != 1) Timber.w("Unexpected args for $name: $args")
                        callback?.onTetheringFailed(args.getOrNull(0) as? Int?)
                        null
                    }
                    else -> {
                        Timber.w("Unexpected method, calling super: $method")
                        ProxyBuilder.callSuper(proxy, method, args)
                    }
                }
            }
            startTethering.invoke(instance, request, executor, proxy)
            return
        } catch (e: InvocationTargetException) {
            Timber.w(e, "Unable to invoke TetheringManager.startTethering, falling back to ConnectivityManager")
        }
        val proxy = ProxyBuilder.forClass(classOnStartTetheringCallback).apply {
            dexCache(app.deviceStorage.cacheDir)
            handler { proxy, method, args ->
                if (args.isNotEmpty()) Timber.w("Unexpected args for ${method.name}: $args")
                @Suppress("NAME_SHADOWING") val callback = reference.get()
                when (method.name) {
                    "onTetheringStarted" -> {
                        callback?.onTetheringStarted()
                        null
                    }
                    "onTetheringFailed" -> {
                        callback?.onTetheringFailed()
                        null
                    }
                    else -> {
                        Timber.w("Unexpected method, calling super: $method")
                        ProxyBuilder.callSuper(proxy, method, args)
                    }
                }
            }
        }.build()
        startTetheringLegacy.invoke(app.connectivity, type, showProvisioningUi, proxy, handler)
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
    fun stopTethering(type: Int) {
        if (BuildCompat.isAtLeastR()) try {
            stopTethering.invoke(instance, type)
        } catch (e: InvocationTargetException) {
            Timber.w(e, "Unable to invoke TetheringManager.stopTethering, falling back to ConnectivityManager")
        }
        stopTetheringLegacy.invoke(app.connectivity, type)
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

    // tether errors defined in ConnectivityManager up to Android 10
    private val tetherErrors29 = arrayOf("TETHER_ERROR_NO_ERROR", "TETHER_ERROR_UNKNOWN_IFACE",
            "TETHER_ERROR_SERVICE_UNAVAIL", "TETHER_ERROR_UNSUPPORTED", "TETHER_ERROR_UNAVAIL_IFACE",
            "TETHER_ERROR_MASTER_ERROR", "TETHER_ERROR_TETHER_IFACE_ERROR", "TETHER_ERROR_UNTETHER_IFACE_ERROR",
            "TETHER_ERROR_ENABLE_NAT_ERROR", "TETHER_ERROR_DISABLE_NAT_ERROR", "TETHER_ERROR_IFACE_CFG_ERROR",
            "TETHER_ERROR_PROVISION_FAILED", "TETHER_ERROR_DHCPSERVER_ERROR", "TETHER_ERROR_ENTITLEMENT_UNKNOWN")
    @get:RequiresApi(30)
    private val tetherErrors by lazy {
        SparseArrayCompat<String>().apply {
            for (field in clazz.declaredFields) try {
                // all TETHER_ERROR_* are system-api since API 30
                if (field.name.startsWith("TETHER_ERROR_")) put(field.get(null) as Int, field.name)
            } catch (e: Exception) {
                Timber.w(e)
            }
        }
    }
    fun tetherErrorMessage(error: Int): String {
        if (BuildCompat.isAtLeastR()) try {
            tetherErrors.get(error)?.let { return it }
        } catch (e: ReflectiveOperationException) {
            Timber.w(e)
        }
        return tetherErrors29.getOrNull(error) ?: app.getString(R.string.failure_reason_unknown, error)
    }

    val Intent.tetheredIfaces get() = getStringArrayListExtra(
            if (Build.VERSION.SDK_INT >= 26) EXTRA_ACTIVE_TETHER else EXTRA_ACTIVE_TETHER_LEGACY)
    val Intent.localOnlyTetheredIfaces get() = if (Build.VERSION.SDK_INT >= 26) {
        getStringArrayListExtra(
                if (BuildCompat.isAtLeastR()) EXTRA_ACTIVE_LOCAL_ONLY else EXTRA_ACTIVE_LOCAL_ONLY_LEGACY)
    } else emptyList<String>()
}
