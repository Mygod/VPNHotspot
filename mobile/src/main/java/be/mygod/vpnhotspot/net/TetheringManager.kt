package be.mygod.vpnhotspot.net

import android.annotation.TargetApi
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.LinkAddress
import android.net.Network
import android.os.Build
import android.os.Handler
import androidx.annotation.RequiresApi
import androidx.collection.SparseArrayCompat
import androidx.core.os.BuildCompat
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.ensureReceiverUnregistered
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
    @RequiresApi(30)
    const val TETHERING_SERVICE = "tethering"

    @RequiresApi(30)
    const val PACKAGE = "com.android.networkstack.tethering"

    @RequiresApi(30)
    private const val TETHERING_CONNECTOR_CLASS = "android.net.ITetheringConnector"

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

    /** Tethering offload status is stopped.  */
    @RequiresApi(30)
    const val TETHER_HARDWARE_OFFLOAD_STOPPED = 0
    /** Tethering offload status is started.  */
    @RequiresApi(30)
    const val TETHER_HARDWARE_OFFLOAD_STARTED = 1
    /** Fail to start tethering offload.  */
    @RequiresApi(30)
    const val TETHER_HARDWARE_OFFLOAD_FAILED = 2

    // tethering types supported by enableTetheringInternal: https://android.googlesource.com/platform/frameworks/base/+/5d36f01/packages/Tethering/src/com/android/networkstack/tethering/Tethering.java#549
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
    /**
     * Ncm local tethering type.
     *
     * Requires NETWORK_SETTINGS permission, which is sadly not obtainable.
     * @see [startTethering]
     */
    @RequiresApi(30)
    const val TETHERING_NCM = 4
    /**
     * Ethernet tethering type.
     *
     * Requires MANAGE_USB permission, also.
     * @see [startTethering]
     */
    @RequiresApi(30)
    const val TETHERING_ETHERNET = 5

    @get:RequiresApi(30)
    private val clazz by lazy { Class.forName("android.net.TetheringManager") }
    @get:RequiresApi(30)
    private val instance by lazy @TargetApi(30) { app.getSystemService(TETHERING_SERVICE) }
    @get:RequiresApi(30)
    val resolvedService by lazy @TargetApi(30) {
        app.packageManager.queryIntentServices(Intent(TETHERING_CONNECTOR_CLASS),
                PackageManager.MATCH_SYSTEM_ONLY).single()
    }

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

    private fun Handler?.makeExecutor() = Executor { if (this == null) it.run() else post(it) }

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
                build.invoke(builder)
            }
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
            startTethering.invoke(instance, request, handler.makeExecutor(), proxy)
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
     * Callback for use with [registerTetheringEventCallback] to find out tethering
     * upstream status.
     */
    interface TetheringEventCallback {
        /**
         * Called when tethering supported status changed.
         *
         * This will be called immediately after the callback is registered, and may be called
         * multiple times later upon changes.
         *
         * Tethering may be disabled via system properties, device configuration, or device
         * policy restrictions.
         *
         * @param supported The new supported status
         */
        fun onTetheringSupported(supported: Boolean) {}

        /**
         * Called when tethering upstream changed.
         *
         * This will be called immediately after the callback is registered, and may be called
         * multiple times later upon changes.
         *
         * @param network the [Network] of tethering upstream. Null means tethering doesn't
         * have any upstream.
         */
        fun onUpstreamChanged(network: Network?) {}

        /**
         * Called when there was a change in tethering interface regular expressions.
         *
         * This will be called immediately after the callback is registered, and may be called
         * multiple times later upon changes.
         *
         * *@param reg The new regular expressions.
         * @hide
         */
        fun onTetherableInterfaceRegexpsChanged() {}

        /**
         * Called when there was a change in the list of tetherable interfaces. Tetherable
         * interface means this interface is available and can be used for tethering.
         *
         * This will be called immediately after the callback is registered, and may be called
         * multiple times later upon changes.
         * @param interfaces The list of tetherable interface names.
         */
        fun onTetherableInterfacesChanged(interfaces: List<String?>) {}

        /**
         * Called when there was a change in the list of tethered interfaces.
         *
         * This will be called immediately after the callback is registered, and may be called
         * multiple times later upon changes.
         * @param interfaces The list of 0 or more String of currently tethered interface names.
         */
        fun onTetheredInterfacesChanged(interfaces: List<String?>) {}

        /**
         * Called when an error occurred configuring tethering.
         *
         * This will be called immediately after the callback is registered if the latest status
         * on the interface is an error, and may be called multiple times later upon changes.
         * @param ifName Name of the interface.
         * @param error One of `TetheringManager#TETHER_ERROR_*`.
         */
        fun onError(ifName: String, error: Int) {}

        /**
         * Called when the list of tethered clients changes.
         *
         * This callback provides best-effort information on connected clients based on state
         * known to the system, however the list cannot be completely accurate (and should not be
         * used for security purposes). For example, clients behind a bridge and using static IP
         * assignments are not visible to the tethering device; or even when using DHCP, such
         * clients may still be reported by this callback after disconnection as the system cannot
         * determine if they are still connected.
         *
         * Only called if having permission one of NETWORK_SETTINGS, MAINLINE_NETWORK_STACK, NETWORK_STACK.
         * @param clients The new set of tethered clients; the collection is not ordered.
         */
        fun onClientsChanged(clients: Iterable<*>) {
            Timber.i("onClientsChanged: ${clients.joinToString()}")
        }

        /**
         * Called when tethering offload status changes.
         *
         * This will be called immediately after the callback is registered.
         * @param status The offload status.
         */
        fun onOffloadStatusChanged(status: Int) {}
    }

    @get:RequiresApi(30)
    private val interfaceTetheringEventCallback by lazy {
        Class.forName("android.net.TetheringManager\$TetheringEventCallback")
    }
    @get:RequiresApi(30)
    private val registerTetheringEventCallback by lazy {
        clazz.getDeclaredMethod("registerTetheringEventCallback", Executor::class.java, interfaceTetheringEventCallback)
    }
    @get:RequiresApi(30)
    private val unregisterTetheringEventCallback by lazy {
        clazz.getDeclaredMethod("unregisterTetheringEventCallback", interfaceTetheringEventCallback)
    }

    private val callbackMap = mutableMapOf<TetheringEventCallback, Any>()
    /**
     * Start listening to tethering change events. Any new added callback will receive the last
     * tethering status right away. If callback is registered,
     * [TetheringEventCallback.onUpstreamChanged] will immediately be called. If tethering
     * has no upstream or disabled, the argument of callback will be null. The same callback object
     * cannot be registered twice.
     *
     * Requires TETHER_PRIVILEGED or ACCESS_NETWORK_STATE.
     *
     * @param executor the executor on which callback will be invoked.
     * @param callback the callback to be called when tethering has change events.
     */
    @RequiresApi(30)
    fun registerTetheringEventCallback(executor: Executor, callback: TetheringEventCallback) {
        val reference = WeakReference(callback)
        val proxy = synchronized(callbackMap) {
            callbackMap.computeIfAbsent(callback) {
                Proxy.newProxyInstance(interfaceTetheringEventCallback.classLoader,
                        arrayOf(interfaceTetheringEventCallback)) { proxy, method, args ->
                    @Suppress("NAME_SHADOWING") val callback = reference.get()
                    when (val name = method.name) {
                        "onTetheringSupported" -> {
                            if (args.size > 1) Timber.w("Unexpected args for $name: $args")
                            callback?.onTetheringSupported(args[0] as Boolean)
                            null
                        }
                        "onUpstreamChanged" -> {
                            if (args.size > 1) Timber.w("Unexpected args for $name: $args")
                            callback?.onUpstreamChanged(args[0] as Network?)
                            null
                        }
                        "onTetherableInterfaceRegexpsChanged" -> {
                            callback?.onTetherableInterfaceRegexpsChanged()
                            null
                        }
                        "onTetherableInterfacesChanged" -> {
                            if (args.size > 1) Timber.w("Unexpected args for $name: $args")
                            @Suppress("UNCHECKED_CAST")
                            callback?.onTetherableInterfacesChanged(args[0] as List<String?>)
                            null
                        }
                        "onTetheredInterfacesChanged" -> {
                            if (args.size > 1) Timber.w("Unexpected args for $name: $args")
                            @Suppress("UNCHECKED_CAST")
                            callback?.onTetheredInterfacesChanged(args[0] as List<String?>)
                            null
                        }
                        "onError" -> {
                            if (args.size > 2) Timber.w("Unexpected args for $name: $args")
                            callback?.onError(args[0] as String, args[1] as Int)
                            null
                        }
                        "onClientsChanged" -> {
                            if (args.size > 1) Timber.w("Unexpected args for $name: $args")
                            callback?.onClientsChanged(args[0] as Iterable<*>)
                            null
                        }
                        "onOffloadStatusChanged" -> {
                            if (args.size > 1) Timber.w("Unexpected args for $name: $args")
                            callback?.onOffloadStatusChanged(args[0] as Int)
                            null
                        }
                        else -> {
                            Timber.w("Unexpected method, calling super: $method")
                            ProxyBuilder.callSuper(proxy, method, args)
                        }
                    }
                }
            }
        }
        registerTetheringEventCallback.invoke(instance, executor, proxy)
    }
    /**
     * Remove tethering event callback previously registered with
     * [registerTetheringEventCallback].
     *
     * Requires TETHER_PRIVILEGED or ACCESS_NETWORK_STATE.
     *
     * @param callback previously registered callback.
     */
    @RequiresApi(30)
    fun unregisterTetheringEventCallback(callback: TetheringEventCallback) {
        val proxy = synchronized(callbackMap) { callbackMap.remove(callback) } ?: return
        unregisterTetheringEventCallback.invoke(instance, proxy)
    }

    /**
     * [registerTetheringEventCallback] in a backwards compatible way.
     *
     * Only [TetheringEventCallback.onTetheredInterfacesChanged] is supported on API 29-.
     */
    fun registerTetheringEventCallbackCompat(context: Context, callback: TetheringEventCallback) {
        if (BuildCompat.isAtLeastR()) {
            registerTetheringEventCallback(null.makeExecutor(), callback)
        } else synchronized(callbackMap) {
            callbackMap.computeIfAbsent(callback) {
                broadcastReceiver { _, intent ->
                    callback.onTetheredInterfacesChanged(intent.tetheredIfaces ?: return@broadcastReceiver)
                }.also { context.registerReceiver(it, IntentFilter(ACTION_TETHER_STATE_CHANGED)) }
            }
        }
    }
    fun unregisterTetheringEventCallbackCompat(context: Context, callback: TetheringEventCallback) {
        if (BuildCompat.isAtLeastR()) {
            unregisterTetheringEventCallback(callback)
        } else {
            val receiver = synchronized(callbackMap) { callbackMap.remove(callback) } ?: return
            context.ensureReceiverUnregistered(receiver as BroadcastReceiver)
        }
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
