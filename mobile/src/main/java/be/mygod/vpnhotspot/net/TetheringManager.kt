package be.mygod.vpnhotspot.net

import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.Network
import android.os.Build
import android.os.Handler
import androidx.annotation.RequiresApi
import androidx.core.os.ExecutorCompat
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.StartTethering
import be.mygod.vpnhotspot.root.StopTethering
import be.mygod.vpnhotspot.util.*
import com.android.dx.stock.ProxyBuilder
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber
import java.io.File
import java.lang.ref.WeakReference
import java.lang.reflect.InvocationHandler
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Method
import java.lang.reflect.Proxy
import java.util.concurrent.CancellationException
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

        /**
         * ADDED: Called when a local Exception occurred.
         */
        fun onException(e: Exception) {
            when (e.getRootCause()) {
                is SecurityException, is CancellationException -> { }
                else -> Timber.w(e)
            }
        }
    }

    private object InPlaceExecutor : Executor {
        override fun execute(command: Runnable) = try {
            command.run()
        } catch (e: Exception) {
            Timber.w(e) // prevent Binder stub swallowing the exception
        }
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
    private const val TETHERING_CONNECTOR_CLASS = "android.net.ITetheringConnector"
    @RequiresApi(30)
    private const val IN_PROCESS_SUFFIX = ".InProcess"

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
     * @see startTethering
     */
    @RequiresApi(24)
    const val TETHERING_USB = 1
    /**
     * Bluetooth tethering type.
     *
     * Requires BLUETOOTH permission.
     * @see startTethering
     */
    @RequiresApi(24)
    const val TETHERING_BLUETOOTH = 2
    /**
     * Ncm local tethering type.
     *
     * @see startTethering
     */
    @RequiresApi(30)
    const val TETHERING_NCM = 4
    /**
     * Ethernet tethering type.
     *
     * Requires MANAGE_USB permission, also.
     * @see startTethering
     */
    @RequiresApi(30)
    const val TETHERING_ETHERNET = 5

    @get:RequiresApi(30)
    private val clazz by lazy { Class.forName("android.net.TetheringManager") }
    @get:RequiresApi(30)
    private val instance by lazy @TargetApi(30) {
        @SuppressLint("WrongConstant")      // hidden services are not included in constants as of R preview 4
        val service = Services.context.getSystemService(TETHERING_SERVICE)
        service
    }

    @get:RequiresApi(30)
    val resolvedService get() = sequence {
        for (action in arrayOf(TETHERING_CONNECTOR_CLASS + IN_PROCESS_SUFFIX, TETHERING_CONNECTOR_CLASS)) {
            val result = app.packageManager.queryIntentServices(Intent(action), PackageManager.MATCH_SYSTEM_ONLY)
            check(result.size <= 1) { "Multiple system services handle $action: ${result.joinToString()}" }
            result.firstOrNull()?.let { yield(it) }
        }
    }.first()

    @get:RequiresApi(24)
    private val classOnStartTetheringCallback by lazy {
        Class.forName("android.net.ConnectivityManager\$OnStartTetheringCallback")
    }
    @get:RequiresApi(24)
    private val startTetheringLegacy by lazy @TargetApi(24) {
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
    private val newTetheringRequestBuilder by lazy @TargetApi(30) {
        classTetheringRequestBuilder.getConstructor(Int::class.java)
    }
//    @get:RequiresApi(30)
//    private val setStaticIpv4Addresses by lazy {
//        classTetheringRequestBuilder.getDeclaredMethod("setStaticIpv4Addresses",
//                LinkAddress::class.java, LinkAddress::class.java)
//    }
    @get:RequiresApi(30)
    private val setExemptFromEntitlementCheck by lazy @TargetApi(30) {
        classTetheringRequestBuilder.getDeclaredMethod("setExemptFromEntitlementCheck", Boolean::class.java)
    }
    @get:RequiresApi(30)
    private val setShouldShowEntitlementUi by lazy @TargetApi(30) {
        classTetheringRequestBuilder.getDeclaredMethod("setShouldShowEntitlementUi", Boolean::class.java)
    }
    @get:RequiresApi(30)
    private val build by lazy @TargetApi(30) { classTetheringRequestBuilder.getDeclaredMethod("build") }

    @get:RequiresApi(30)
    private val interfaceStartTetheringCallback by lazy {
        Class.forName("android.net.TetheringManager\$StartTetheringCallback")
    }
    @get:RequiresApi(30)
    private val startTethering by lazy @TargetApi(30) {
        clazz.getDeclaredMethod("startTethering", Class.forName("android.net.TetheringManager\$TetheringRequest"),
                Executor::class.java, interfaceStartTetheringCallback)
    }
    @get:RequiresApi(30)
    private val stopTethering by lazy @TargetApi(30) { clazz.getDeclaredMethod("stopTethering", Int::class.java) }

    @Deprecated("Legacy API")
    @RequiresApi(24)
    fun startTetheringLegacy(type: Int, showProvisioningUi: Boolean, callback: StartTetheringCallback,
                             handler: Handler? = null, cacheDir: File = app.deviceStorage.codeCacheDir) {
        val reference = WeakReference(callback)
        val proxy = ProxyBuilder.forClass(classOnStartTetheringCallback).apply {
            dexCache(cacheDir)
            handler { proxy, method, args ->
                @Suppress("NAME_SHADOWING") val callback = reference.get()
                if (args.isEmpty()) when (method.name) {
                    "onTetheringStarted" -> return@handler callback?.onTetheringStarted()
                    "onTetheringFailed" -> return@handler callback?.onTetheringFailed()
                }
                ProxyBuilder.callSuper(proxy, method, args)
            }
        }.build()
        startTetheringLegacy(Services.connectivity, type, showProvisioningUi, proxy, handler)
    }
    @RequiresApi(30)
    fun startTethering(type: Int, exemptFromEntitlementCheck: Boolean, showProvisioningUi: Boolean, executor: Executor,
                       proxy: Any) {
        startTethering(instance, newTetheringRequestBuilder.newInstance(type).let { builder ->
            // setting exemption requires TETHER_PRIVILEGED permission
            if (exemptFromEntitlementCheck) setExemptFromEntitlementCheck(builder, true)
            setShouldShowEntitlementUi(builder, showProvisioningUi)
            build(builder)
        }, executor, proxy)
    }
    @RequiresApi(30)
    fun proxy(callback: StartTetheringCallback): Any {
        val reference = WeakReference(callback)
        return Proxy.newProxyInstance(interfaceStartTetheringCallback.classLoader,
                arrayOf(interfaceStartTetheringCallback), object : InvocationHandler {
            override fun invoke(proxy: Any, method: Method, args: Array<out Any?>?): Any? {
                @Suppress("NAME_SHADOWING") val callback = reference.get()
                return when {
                    method.matches("onTetheringStarted") -> callback?.onTetheringStarted()
                    method.matches("onTetheringFailed", Integer.TYPE) -> {
                        callback?.onTetheringFailed(args?.get(0) as Int)
                    }
                    else -> callSuper(interfaceStartTetheringCallback, proxy, method, args)
                }
            }
        })
    }
    /**
     * Runs tether provisioning for the given type if needed and then starts tethering if
     * the check succeeds. If no carrier provisioning is required for tethering, tethering is
     * enabled immediately. If provisioning fails, tethering will not be enabled. It also
     * schedules tether provisioning re-checks if appropriate.
     *
     * CHANGED BEHAVIOR: This method will not throw Exceptions, instead, callback.onException will be called.
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
     * *@param localIPv4Address for API 30+ (not implemented for now due to blacklist). If present, it
     *         configures tethering with the preferred local IPv4 link address to use.
     * *@see setStaticIpv4Addresses
     */
    @RequiresApi(24)
    fun startTethering(type: Int, showProvisioningUi: Boolean, callback: StartTetheringCallback,
                       handler: Handler? = null, cacheDir: File = app.deviceStorage.codeCacheDir) {
        if (Build.VERSION.SDK_INT >= 30) try {
            val executor = if (handler == null) InPlaceExecutor else ExecutorCompat.create(handler)
            startTethering(type, true, showProvisioningUi,
                    executor, proxy(object : StartTetheringCallback {
                override fun onTetheringStarted() = callback.onTetheringStarted()
                override fun onTetheringFailed(error: Int?) {
                    if (error != TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION) callback.onTetheringFailed(error)
                    else GlobalScope.launch(Dispatchers.Unconfined) {
                        val result = try {
                            RootManager.use { it.execute(StartTethering(type, showProvisioningUi)) }
                        } catch (eRoot: Exception) {
                            try {   // last resort: start tethering without trying to bypass entitlement check
                                startTethering(type, false, showProvisioningUi, executor, proxy(callback))
                                if (eRoot !is CancellationException) Timber.w(eRoot)
                            } catch (e: Exception) {
                                e.addSuppressed(eRoot)
                                callback.onException(e)
                            }
                            return@launch
                        }
                        when {
                            result == null -> callback.onTetheringStarted()
                            result.value == TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION -> try {
                                startTethering(type, false, showProvisioningUi, executor, proxy(callback))
                            } catch (e: Exception) {
                                callback.onException(e)
                            }
                            else -> callback.onTetheringFailed(result.value)
                        }
                    }
                }
                override fun onException(e: Exception) = callback.onException(e)
            }))
        } catch (e: Exception) {
            callback.onException(e)
        } else @Suppress("DEPRECATION") try {
            startTetheringLegacy(type, showProvisioningUi, callback, handler, cacheDir)
        } catch (e: InvocationTargetException) {
            if (e.targetException is SecurityException) GlobalScope.launch(Dispatchers.Unconfined) {
                val result = try {
                    val rootCache = File(cacheDir, "root")
                    rootCache.mkdirs()
                    check(rootCache.exists()) { "Creating root cache dir failed" }
                    RootManager.use {
                        it.execute(be.mygod.vpnhotspot.root.StartTetheringLegacy(
                                rootCache, type, showProvisioningUi))
                    }.value
                } catch (eRoot: Exception) {
                    eRoot.addSuppressed(e)
                    if (eRoot !is CancellationException) Timber.w(eRoot)
                    callback.onException(eRoot)
                    return@launch
                }
                if (result) callback.onTetheringStarted() else callback.onTetheringFailed()
            } else callback.onException(e)
        } catch (e: Exception) {
            callback.onException(e)
        }
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
        if (Build.VERSION.SDK_INT >= 30) stopTethering(instance, type)
        else stopTetheringLegacy(Services.connectivity, type)
    }
    @RequiresApi(24)
    fun stopTethering(type: Int, callback: (Exception) -> Unit) {
        try {
            stopTethering(type)
        } catch (e: InvocationTargetException) {
            if (e.targetException is SecurityException) GlobalScope.launch(Dispatchers.Unconfined) {
                try {
                    RootManager.use { it.execute(StopTethering(type)) }
                } catch (eRoot: Exception) {
                    eRoot.addSuppressed(e)
                    callback(eRoot)
                }
            } else callback(e)
        }
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
         * CHANGED: This method will NOT be immediately called after registration.
         *
         * *@param reg The new regular expressions.
         * @hide
         */
        fun onTetherableInterfaceRegexpsChanged(reg: Any?) {}

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
        fun onClientsChanged(clients: Collection<*>) {
            if (clients.isNotEmpty()) Timber.i("onClientsChanged: ${clients.joinToString()}")
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
    private val registerTetheringEventCallback by lazy @TargetApi(30) {
        clazz.getDeclaredMethod("registerTetheringEventCallback", Executor::class.java, interfaceTetheringEventCallback)
    }
    @get:RequiresApi(30)
    private val unregisterTetheringEventCallback by lazy @TargetApi(30) {
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
    fun registerTetheringEventCallback(executor: Executor?, callback: TetheringEventCallback) {
        val reference = WeakReference(callback)
        val proxy = synchronized(callbackMap) {
            var computed = false
            callbackMap.computeIfAbsent(callback) {
                computed = true
                Proxy.newProxyInstance(interfaceTetheringEventCallback.classLoader,
                        arrayOf(interfaceTetheringEventCallback), object : InvocationHandler {
                    private var regexpsSent = false
                    override fun invoke(proxy: Any, method: Method, args: Array<out Any?>?): Any? {
                        @Suppress("NAME_SHADOWING")
                        val callback = reference.get()
                        return when {
                            method.matches("onTetheringSupported", Boolean::class.java) -> {
                                callback?.onTetheringSupported(args!![0] as Boolean)
                            }
                            method.matches1<Network>("onUpstreamChanged") -> {
                                callback?.onUpstreamChanged(args!![0] as Network?)
                            }
                            method.name == "onTetherableInterfaceRegexpsChanged" &&
                                    method.parameters.singleOrNull()?.type?.name ==
                                    "android.net.TetheringManager\$TetheringInterfaceRegexps" -> {
                                if (regexpsSent) callback?.onTetherableInterfaceRegexpsChanged(args!!.single())
                                regexpsSent = true
                            }
                            method.matches1<java.util.List<*>>("onTetherableInterfacesChanged") -> {
                                @Suppress("UNCHECKED_CAST")
                                callback?.onTetherableInterfacesChanged(args!![0] as List<String?>)
                            }
                            method.matches1<java.util.List<*>>("onTetheredInterfacesChanged") -> {
                                @Suppress("UNCHECKED_CAST")
                                callback?.onTetheredInterfacesChanged(args!![0] as List<String?>)
                            }
                            method.matches("onError", String::class.java, Integer.TYPE) -> {
                                callback?.onError(args!![0] as String, args[1] as Int)
                            }
                            method.matches1<java.util.Collection<*>>("onClientsChanged") -> {
                                callback?.onClientsChanged(args!![0] as Collection<*>)
                            }
                            method.matches("onOffloadStatusChanged", Integer.TYPE) -> {
                                callback?.onOffloadStatusChanged(args!![0] as Int)
                            }
                            else -> callSuper(interfaceTetheringEventCallback, proxy, method, args)
                        }
                    }
                })
            }.also { if (!computed) return }
        }
        registerTetheringEventCallback(instance, executor ?: InPlaceExecutor, proxy)
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
        unregisterTetheringEventCallback(instance, proxy)
    }

    /**
     * [registerTetheringEventCallback] in a backwards compatible way.
     *
     * Only [TetheringEventCallback.onTetheredInterfacesChanged] is supported on API 29-.
     */
    fun registerTetheringEventCallbackCompat(context: Context, callback: TetheringEventCallback) {
        if (Build.VERSION.SDK_INT < 30) synchronized(callbackMap) {
            callbackMap.computeIfAbsent(callback) {
                broadcastReceiver { _, intent ->
                    callback.onTetheredInterfacesChanged(intent.tetheredIfaces ?: return@broadcastReceiver)
                }.also { context.registerReceiver(it, IntentFilter(ACTION_TETHER_STATE_CHANGED)) }
            }
        } else registerTetheringEventCallback(InPlaceExecutor, callback)
    }
    fun unregisterTetheringEventCallbackCompat(context: Context, callback: TetheringEventCallback) {
        if (Build.VERSION.SDK_INT < 30) {
            val receiver = synchronized(callbackMap) { callbackMap.remove(callback) } ?: return
            context.ensureReceiverUnregistered(receiver as BroadcastReceiver)
        } else unregisterTetheringEventCallback(callback)
    }

    /**
     * Get a more detailed error code after a Tethering or Untethering
     * request asynchronously failed.
     *
     * @param iface The name of the interface of interest
     * @return error The error code of the last error tethering or untethering the named
     *               interface
     */
    @Deprecated("Use {@link TetheringEventCallback#onError(String, int)} instead.")
    fun getLastTetherError(iface: String): Int = getLastTetherError(Services.connectivity, iface) as Int

    val tetherErrorLookup = ConstantLookup("TETHER_ERROR_",
            "TETHER_ERROR_NO_ERROR", "TETHER_ERROR_UNKNOWN_IFACE", "TETHER_ERROR_SERVICE_UNAVAIL",
            "TETHER_ERROR_UNSUPPORTED", "TETHER_ERROR_UNAVAIL_IFACE", "TETHER_ERROR_MASTER_ERROR",
            "TETHER_ERROR_TETHER_IFACE_ERROR", "TETHER_ERROR_UNTETHER_IFACE_ERROR", "TETHER_ERROR_ENABLE_NAT_ERROR",
            "TETHER_ERROR_DISABLE_NAT_ERROR", "TETHER_ERROR_IFACE_CFG_ERROR", "TETHER_ERROR_PROVISION_FAILED",
            "TETHER_ERROR_DHCPSERVER_ERROR", "TETHER_ERROR_ENTITLEMENT_UNKNOWN") @TargetApi(30) { clazz }
    @RequiresApi(30)
    const val TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION = 14

    val Intent.tetheredIfaces get() = getStringArrayListExtra(
            if (Build.VERSION.SDK_INT >= 26) EXTRA_ACTIVE_TETHER else EXTRA_ACTIVE_TETHER_LEGACY)
    val Intent.localOnlyTetheredIfaces get() = if (Build.VERSION.SDK_INT >= 26) {
        getStringArrayListExtra(
                if (Build.VERSION.SDK_INT >= 30) EXTRA_ACTIVE_LOCAL_ONLY else EXTRA_ACTIVE_LOCAL_ONLY_LEGACY)
    } else emptyList<String>()
}
