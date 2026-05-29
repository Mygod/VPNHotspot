package be.mygod.vpnhotspot.net

import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.IIntResultListener
import android.net.ITetheringConnector
import android.net.Network
import android.net.TetheredClient
import android.net.TetheringInterface
import android.net.TetheringManager
import android.net.`ConnectivityManager$OnStartTetheringCallback`
import android.net.`TetheringManager$TetheringInterfaceRegexps`
import android.os.Build
import android.os.DeadObjectException
import android.os.Handler
import androidx.annotation.Keep
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.TetheringCommands
import be.mygod.vpnhotspot.util.ConstantLookup
import be.mygod.vpnhotspot.util.InPlaceExecutor
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import be.mygod.vpnhotspot.util.callSuper
import be.mygod.vpnhotspot.util.getRootCause
import be.mygod.vpnhotspot.util.matches
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.suspendCancellableCoroutine
import timber.log.Timber
import java.lang.ref.WeakReference
import java.lang.reflect.InvocationHandler
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Method
import java.lang.reflect.Proxy
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CancellationException
import java.util.concurrent.Executor
import kotlin.coroutines.coroutineContext
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

/**
 * Heavily based on:
 * https://github.com/aegis1980/WifiHotSpot
 * https://android.googlesource.com/platform/frameworks/base.git/+/android-7.0.0_r1/core/java/android/net/ConnectivityManager.java
 */
object TetheringManagerCompat {
    /**
     * Thrown by [startTethering]/[stopTethering] when the platform reports that the tethering
     * operation failed. [errorCode] is one of `TetheringManager.TETHER_ERROR_*` (see
     * [tetherErrorLookup]); it is null only for the legacy API, which does not report a code.
     */
    class Failure(val errorCode: Int? = null, cause: Throwable? = null) :
        Exception("Tethering operation failed${errorCode?.let { " (error $it)" }.orEmpty()}", cause)

    /**
     * Log [e] unless it is an expected permission denial or cancellation. Replaces the old
     * `TetheringCallback.onException` default so call sites keep the same quiet-on-SecurityException
     * behavior in their catch blocks. Rethrow [CancellationException] before calling this.
     */
    fun reportException(e: Throwable) = when (e.getRootCause()) {
        is SecurityException, is CancellationException -> { }
        else -> Timber.w(e)
    }

    @RequiresApi(30)
    private const val TETHERING_CONNECTOR_CLASS = "android.net.ITetheringConnector"
    @RequiresApi(30)
    private const val IN_PROCESS_SUFFIX = ".InProcess"

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
     * USB tethering type.
     *
     * Requires MANAGE_USB permission, unfortunately.
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/7ca5d3a/services/usb/java/com/android/server/usb/UsbService.java#389
     * @see startTethering
     */
    const val TETHERING_USB = 1
    /**
     * Bluetooth tethering type.
     *
     * Requires BLUETOOTH permission.
     * @see startTethering
     */
    const val TETHERING_BLUETOOTH = 2
    /**
     * Ethernet tethering type.
     *
     * Requires MANAGE_USB permission, also.
     * @see startTethering
     */
    @RequiresApi(30)
    const val TETHERING_ETHERNET = 5

    @get:RequiresApi(30)
    val resolvedService get() = sequence {
        for (action in arrayOf(TETHERING_CONNECTOR_CLASS + IN_PROCESS_SUFFIX, TETHERING_CONNECTOR_CLASS)) {
            val result = app.packageManager.queryIntentServices(Intent(action), PackageManager.MATCH_SYSTEM_ONLY)
            check(result.size <= 1) { "Multiple system services handle $action: ${result.joinToString()}" }
            result.firstOrNull()?.let { yield(it) }
        }
    }.first()

    private val startTethering by lazy {
        ConnectivityManager::class.java.getDeclaredMethod("startTethering", Int::class.java, Boolean::class.java,
            `ConnectivityManager$OnStartTetheringCallback`::class.java, Handler::class.java)
    }
    private val stopTethering by lazy {
        ConnectivityManager::class.java.getDeclaredMethod("stopTethering", Int::class.java)
    }
    private val getLastTetherError by lazy @SuppressLint("SoonBlockedPrivateApi") {
        ConnectivityManager::class.java.getDeclaredMethod("getLastTetherError", String::class.java)
    }
    private var stopTetheringHasAttributionTag = true

//    @get:RequiresApi(30)
//    private val setStaticIpv4Addresses by lazy {
//        TetheringManager.TetheringRequest.Builder::class.java.getDeclaredMethod("setStaticIpv4Addresses",
//                LinkAddress::class.java, LinkAddress::class.java)
//    }
    @get:RequiresApi(30)
    private val setExemptFromEntitlementCheck by lazy @TargetApi(30) {
        TetheringManager.TetheringRequest.Builder::class.java.getDeclaredMethod("setExemptFromEntitlementCheck",
            Boolean::class.java)
    }
    @get:RequiresApi(30)
    private val setShouldShowEntitlementUi by lazy @TargetApi(30) {
        TetheringManager.TetheringRequest.Builder::class.java.getDeclaredMethod("setShouldShowEntitlementUi",
            Boolean::class.java)
    }

    @Deprecated("Legacy API")
    @Suppress("DEPRECATION")
    suspend fun startTetheringLegacy(type: Int, showProvisioningUi: Boolean): Unit =
        suspendCancellableCoroutine { cont ->
            val proxy = object : `ConnectivityManager$OnStartTetheringCallback`() {
                override fun onTetheringStarted() = cont.resume(Unit)
                override fun onTetheringFailed() = cont.resumeWithException(Failure())
            }
            startTethering(Services.connectivity, type, showProvisioningUi, proxy, null)
        }
    @RequiresApi(30)
    suspend fun startTethering(type: Int, exemptFromEntitlementCheck: Boolean, showProvisioningUi: Boolean): Unit =
        suspendCancellableCoroutine { cont ->
            Services.tethering.startTethering(TetheringManager.TetheringRequest.Builder(type).also { builder ->
                // setting exemption requires TETHER_PRIVILEGED permission
                if (exemptFromEntitlementCheck) setExemptFromEntitlementCheck(builder, true)
                setShouldShowEntitlementUi(builder, showProvisioningUi)
            }.build(), InPlaceExecutor, object : TetheringManager.StartTetheringCallback {
                override fun onTetheringStarted() = cont.resume(Unit)
                override fun onTetheringFailed(error: Int) = cont.resumeWithException(Failure(error))
            })
        }
    /**
     * Runs tether provisioning for the given type if needed and then starts tethering if
     * the check succeeds. If no carrier provisioning is required for tethering, tethering is
     * enabled immediately. If provisioning fails, tethering will not be enabled. It also
     * schedules tether provisioning re-checks if appropriate.
     *
     * CHANGED BEHAVIOR: suspends until done; returns normally on success, throws [Failure] when the
     * platform reports a tethering error, or rethrows the local exception otherwise (callers can
     * route it through [reportException] to preserve the old quiet-on-SecurityException logging).
     *
     * @param type The type of tethering to start. Must be one of
     *         {@link ConnectivityManager.TETHERING_WIFI},
     *         {@link ConnectivityManager.TETHERING_USB}, or
     *         {@link ConnectivityManager.TETHERING_BLUETOOTH}.
     * @param showProvisioningUi a boolean indicating to show the provisioning app UI if there
     *         is one. This should be true the first time this function is called and also any time
     *         the user can see this UI. It gives users information from their carrier about the
     *         check failing and how they can sign up for tethering if possible.
     * *@param localIPv4Address for API 30+ (not implemented for now due to blacklist). If present, it
     *         configures tethering with the preferred local IPv4 link address to use.
     * *@see setStaticIpv4Addresses
     */
    suspend fun startTethering(type: Int, showProvisioningUi: Boolean) {
        if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
            try {
                startTetheringLegacy(type, showProvisioningUi)
            } catch (e: InvocationTargetException) {
                if (e.targetException is SecurityException) try {
                    RootManager.use { it.execute(TetheringCommands.StartLegacy(type, showProvisioningUi)) }
                } catch (eRoot: Exception) {
                    // surface a root-reported Failure as itself (unwrapped from RemoteException)
                    throw (eRoot.getRootCause() as? Failure ?: eRoot).apply { addSuppressed(e) }
                } else throw e
            }
            return
        }
        try {
            return startTethering(type, true, showProvisioningUi)
        } catch (e: Failure) {
            if (e.errorCode != TetheringManager.TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION) throw e
        }
        try {
            RootManager.use { it.execute(TetheringCommands.Start(type, showProvisioningUi)) }
        } catch (eRoot: Exception) {
            // Our own coroutine was cancelled (not the root session): abort instead of issuing a
            // framework start the caller no longer wants.
            coroutineContext.ensureActive()
            val cause = eRoot.getRootCause()
            // root reached the platform but the start was rejected
            if (cause is Failure) {
                if (cause.errorCode == TetheringManager.TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION) {
                    startTethering(type, false, showProvisioningUi)
                } else throw cause
            } else {
                // root could not run at all: last resort without bypassing the entitlement check
                try {
                    startTethering(type, false, showProvisioningUi)
                } catch (e: Failure) {
                    if (eRoot !is CancellationException) Timber.w(eRoot)
                    throw e
                } catch (e: Exception) {
                    throw e.apply { addSuppressed(eRoot) }
                }
                if (eRoot !is CancellationException) Timber.w(eRoot)
            }
        }
    }

    @RequiresApi(30)
    suspend fun stopTethering(type: Int, context: Context): Unit = suspendCancellableCoroutine { cont ->
        val handler = object : InvocationHandler {
            override fun invoke(proxy: Any, method: Method, args: Array<out Any?>?) = when {
                method.matches("onConnectorAvailable", ITetheringConnector::class.java) -> run {
                    val connector = args!![0] as ITetheringConnector
                    val resultListener = object : IIntResultListener.Stub() {
                        override fun onResult(resultCode: Int) {
                            if (resultCode == TetheringManager.TETHER_ERROR_NO_ERROR) cont.resume(Unit)
                            else cont.resumeWithException(Failure(resultCode))
                        }
                    }
                    if (stopTetheringHasAttributionTag) try {
                        connector.stopTethering(type, context.opPackageName, context.attributionTag, resultListener)
                        return@run
                    } catch (e: NoSuchMethodError) {
                        if (Build.VERSION.SDK_INT >= 31) Timber.w(e)
                        stopTetheringHasAttributionTag = false
                    }
                    connector.stopTethering(type, context.opPackageName, resultListener)
                }
                else -> callSuper(UnblockCentral.TetheringManager_ConnectorConsumer, proxy, method, args)
            }
        }
        UnblockCentral.TetheringManager_getConnector(Services.tethering, Proxy.newProxyInstance(
            UnblockCentral.TetheringManager_ConnectorConsumer.classLoader,
            arrayOf(UnblockCentral.TetheringManager_ConnectorConsumer), handler))
    }
    @RequiresApi(30)
    private suspend fun stopTetheringRoot(type: Int, suppressed: Exception? = null) {
        try {
            RootManager.use { it.execute(TetheringCommands.Stop(type)) }
        } catch (eRoot: Exception) {
            // Our own coroutine was cancelled (not the root session): abort instead of issuing a
            // synchronous legacy stop the caller no longer wants.
            coroutineContext.ensureActive()
            // any root failure (a remote Failure or the root session itself) falls back to the legacy stop
            stopTetheringLegacy(type, if (eRoot is CancellationException) suppressed else eRoot.apply {
                if (suppressed != null) addSuppressed(suppressed)
            })
            return
        }
        if (suppressed != null) Timber.w(suppressed)
    }
    suspend fun stopTethering(type: Int) = if (Build.VERSION.SDK_INT >= 30) try {
        stopTethering(type, app)
    } catch (e: Failure) {
        if (e.errorCode != TetheringManager.TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION) throw e
        stopTetheringRoot(type)
    } catch (e: CancellationException) {
        throw e
    } catch (e: Exception) {
        stopTetheringRoot(type, e)
    } else stopTetheringLegacy(type, null)
    fun stopTetheringLegacy(type: Int) = stopTethering(Services.connectivity, type)
    private suspend fun stopTetheringLegacy(type: Int, suppressed: Exception?) {
        try {
            stopTetheringLegacy(type)
            if (suppressed != null) Timber.w(suppressed)
        } catch (e: InvocationTargetException) {
            if (suppressed != null) e.addSuppressed(suppressed)
            if (e.targetException !is SecurityException) throw e
            try {
                RootManager.use { it.execute(TetheringCommands.StopLegacy(type)) }
            } catch (eRoot: Exception) {
                throw eRoot.apply { addSuppressed(e) }
            }
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
         * Called when tethering supported status changed.
         *
         * This will be called immediately after the callback is registered, and may be called
         * multiple times later upon changes.
         *
         * Tethering may be disabled via system properties, device configuration, or device
         * policy restrictions.
         *
         * @param supportedTypes a set of @TetheringType which is supported.
         */
        fun onSupportedTetheringTypes(supportedTypes: Set<Int?>) {
            val filtered = supportedTypes.filter { it !in 0..5 }
            if (filtered.isNotEmpty()) Timber.w(Exception(
                "Unexpected supported tethering types: ${filtered.joinToString()}"))
        }

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
         * Called when there was a change in the list of local-only interfaces.
         *
         * This is only available from the public callback on newer Mainline releases. API 30
         * runtime variants that still lack this callback should keep using the sticky tether-state
         * broadcast for local-only membership.
         *
         * @param interfaces The list of 0 or more String of active local-only interface names.
         */
        fun onLocalOnlyInterfacesChanged(interfaces: List<String?>) {}

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
        fun onClientsChanged(clients: Collection<TetheredClient>) {
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

    private val tetheringInterfaces = ConcurrentHashMap<String, Int>()
    fun getInterfaceType(iface: String) = tetheringInterfaces[iface]
    @RequiresApi(30)
    private fun toInterfaceCompat(arg: Any?) = (arg as TetheringInterface).let {
        it.`interface`.also { iface -> tetheringInterfaces[iface] = it.type }
    }
    @RequiresApi(30)
    private fun toInterfacesCompat(interfaces: Set<TetheringInterface>) = interfaces.map(this::toInterfaceCompat)

    private val callbackMap = mutableMapOf<TetheringEventCallback, TetheringManager.TetheringEventCallback>()
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
    fun registerTetheringEventCallback(callback: TetheringEventCallback, executor: Executor? = null) {
        val reference = WeakReference(callback)
        val platformCallback = synchronized(callbackMap) {
            var computed = false
            callbackMap.computeIfAbsent(callback) {
                computed = true
                @Keep
                open class LegacyCallback : TetheringManager.TetheringEventCallback {
                    fun onTetheringSupported(supported: Boolean) {
                        reference.get()?.onTetheringSupported(supported)
                    }

                    fun onSupportedTetheringTypes(supportedTypes: Set<Int?>) {
                        reference.get()?.onSupportedTetheringTypes(supportedTypes)
                    }

                    fun onUpstreamChanged(network: Network?) {
                        reference.get()?.onUpstreamChanged(network)
                    }

                    fun onTetherableInterfaceRegexpsChanged(reg: `TetheringManager$TetheringInterfaceRegexps`) {
                        reference.get()?.onTetherableInterfaceRegexpsChanged(reg)
                    }

                    fun onTetherableInterfacesChanged(interfaces: List<String?>) {
                        reference.get()?.onTetherableInterfacesChanged(interfaces)
                    }

                    fun onTetheredInterfacesChanged(interfaces: List<String?>) {
                        reference.get()?.onTetheredInterfacesChanged(interfaces)
                    }

                    fun onLocalOnlyInterfacesChanged(interfaces: List<String?>) {
                        reference.get()?.onLocalOnlyInterfacesChanged(interfaces)
                    }

                    fun onError(ifName: String, error: Int) {
                        reference.get()?.onError(ifName, error)
                    }

                    fun onClientsChanged(clients: Collection<TetheredClient>) {
                        reference.get()?.onClientsChanged(clients)
                    }

                    fun onOffloadStatusChanged(status: Int) {
                        reference.get()?.onOffloadStatusChanged(status)
                    }
                }
                try {
                    @Suppress("UNUSED_VARIABLE", "unused")
                    val clazz = TetheringInterface::class.java
                    object : LegacyCallback() {
                        @Keep
                        fun onTetherableInterfacesChanged(interfaces: Set<TetheringInterface>) {
                            reference.get()?.onTetherableInterfacesChanged(toInterfacesCompat(interfaces))
                        }

                        override fun onTetheredInterfacesChanged(interfaces: Set<TetheringInterface>) {
                            reference.get()?.onTetheredInterfacesChanged(toInterfacesCompat(interfaces))
                        }

                        @Keep
                        fun onLocalOnlyInterfacesChanged(interfaces: Set<TetheringInterface>) {
                            reference.get()?.onLocalOnlyInterfacesChanged(toInterfacesCompat(interfaces))
                        }

                        @Keep
                        fun onError(iface: TetheringInterface, error: Int) {
                            reference.get()?.onError(toInterfaceCompat(iface), error)
                        }
                    }
                } catch (e: NoClassDefFoundError) {
                    if (Build.VERSION.SDK_INT >= 31) Timber.w(e)
                    LegacyCallback()
                }
            }.also { if (!computed) return }
        }
        Services.tethering.registerTetheringEventCallback(executor ?: InPlaceExecutor, platformCallback)
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
        try {
            Services.tethering.unregisterTetheringEventCallback(proxy)
        } catch (e: IllegalStateException) {
            if (e.cause !is DeadObjectException) throw e
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
    @Deprecated("Use {@link TetheringEventCallback#onError(String, int)} instead.")
    fun getLastTetherError(iface: String): Int = getLastTetherError(Services.connectivity, iface) as Int

    val tetherErrorLookup = ConstantLookup("TETHER_ERROR_",
            "NO_ERROR", "UNKNOWN_IFACE", "SERVICE_UNAVAIL", "UNSUPPORTED", "UNAVAIL_IFACE", "MASTER_ERROR",
            "TETHER_IFACE_ERROR", "UNTETHER_IFACE_ERROR", "ENABLE_NAT_ERROR", "DISABLE_NAT_ERROR",
            "IFACE_CFG_ERROR", "PROVISION_FAILED", "DHCPSERVER_ERROR", "ENTITLEMENT_UNKNOWN") @TargetApi(30) {
        TetheringManager::class.java
    }
}
