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
import android.os.Parcelable
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
import be.mygod.vpnhotspot.util.binderCallbackFlow
import be.mygod.vpnhotspot.util.getRootCause
import be.mygod.vpnhotspot.util.matches
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.lang.reflect.InvocationHandler
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Method
import java.lang.reflect.Proxy
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CancellationException
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
     * A tethering change emitted by [eventFlow], mirroring the platform
     * `TetheringManager.TetheringEventCallback` callbacks. Each subtype carries the same arguments the
     * corresponding callback would receive; consumers filter for the events they care about. Most
     * events are also delivered immediately upon (re)registration.
     */
    sealed class Event {
        /** Tethering supported status changed. */
        data class TetheringSupported(val supported: Boolean) : Event()
        /** The set of supported `@TetheringType` values changed. */
        data class SupportedTetheringTypes(val supportedTypes: Set<Int?>) : Event()
        /** Tethering upstream changed; null means there is no upstream. */
        data class UpstreamChanged(val network: Network?) : Event()
        /**
         * Tethering interface regular expressions changed. Unlike the platform callback this is also
         * emitted immediately on registration, so consumers drop the first emission. @hide
         */
        data class TetherableInterfaceRegexpsChanged(val reg: Any?) : Event()
        /** The list of tetherable (available) interface names changed. */
        data class TetherableInterfacesChanged(val interfaces: List<String?>) : Event()
        /** The list of tethered interface names changed. */
        data class TetheredInterfacesChanged(val interfaces: List<String?>) : Event()
        /**
         * The list of active local-only interface names changed. Only delivered by the public callback
         * on newer Mainline releases; API 30 variants without it fall back to the tether-state broadcast.
         */
        data class LocalOnlyInterfacesChanged(val interfaces: List<String?>) : Event()
        /** An error (one of `TetheringManager#TETHER_ERROR_*`) occurred configuring [ifName]. */
        data class ErrorChanged(val ifName: String, val error: Int) : Event()
        /**
         * The set of tethered clients changed (best-effort). Only delivered with one of NETWORK_SETTINGS
         * / MAINLINE_NETWORK_STACK / NETWORK_STACK, i.e. in practice only over root — hence [Parcelable],
         * so it can be forwarded straight across the root boundary.
         */
        @Parcelize
        data class ClientsChanged(val clients: List<TetheredClient>) : Event(), Parcelable
        /** Tethering offload status changed. */
        data class OffloadStatusChanged(val status: Int) : Event()
    }

    private val tetheringInterfaces = ConcurrentHashMap<String, Int>()
    fun getInterfaceType(iface: String) = tetheringInterfaces[iface]
    @RequiresApi(30)
    private fun toInterfaceCompat(arg: Any?) = (arg as TetheringInterface).let {
        it.`interface`.also { iface -> tetheringInterfaces[iface] = it.type }
    }
    @RequiresApi(30)
    private fun toInterfacesCompat(interfaces: Set<TetheringInterface>) = interfaces.map(this::toInterfaceCompat)

    /**
     * Tethering change events as a cold [Flow] — one platform callback registration per collector,
     * replacing the old register/unregisterTetheringEventCallback pair. The registration is bound to
     * collection and torn down (under NonCancellable) when collection ends, and the platform callback
     * only references the flow's channel, so the framework retains nothing heavier than that.
     *
     * Most events are also delivered immediately on registration. Requires TETHER_PRIVILEGED or
     * ACCESS_NETWORK_STATE; [Event.ClientsChanged] additionally needs signature permissions.
     */
    @get:RequiresApi(30)
    val eventFlow: Flow<Event> = binderCallbackFlow("tethering event callback") {
        @Keep
        open class LegacyCallback : TetheringManager.TetheringEventCallback {
            fun onTetheringSupported(supported: Boolean) = push(Event.TetheringSupported(supported)).run { }
            fun onSupportedTetheringTypes(supportedTypes: Set<Int?>) =
                push(Event.SupportedTetheringTypes(supportedTypes)).run { }
            fun onUpstreamChanged(network: Network?) = push(Event.UpstreamChanged(network)).run { }
            fun onTetherableInterfaceRegexpsChanged(reg: `TetheringManager$TetheringInterfaceRegexps`) =
                push(Event.TetherableInterfaceRegexpsChanged(reg)).run { }
            fun onTetherableInterfacesChanged(interfaces: List<String?>) =
                push(Event.TetherableInterfacesChanged(interfaces)).run { }
            fun onTetheredInterfacesChanged(interfaces: List<String?>) =
                push(Event.TetheredInterfacesChanged(interfaces)).run { }
            fun onLocalOnlyInterfacesChanged(interfaces: List<String?>) =
                push(Event.LocalOnlyInterfacesChanged(interfaces)).run { }
            fun onError(ifName: String, error: Int) = push(Event.ErrorChanged(ifName, error)).run { }
            fun onClientsChanged(clients: Collection<TetheredClient>) =
                push(Event.ClientsChanged(clients.toList())).run { }
            fun onOffloadStatusChanged(status: Int) = push(Event.OffloadStatusChanged(status)).run { }
        }
        val callback = try {
            @Suppress("UNUSED_VARIABLE", "unused")
            val clazz = TetheringInterface::class.java
            object : LegacyCallback() {
                @Keep
                fun onTetherableInterfacesChanged(interfaces: Set<TetheringInterface>) =
                    push(Event.TetherableInterfacesChanged(toInterfacesCompat(interfaces))).run { }
                override fun onTetheredInterfacesChanged(interfaces: Set<TetheringInterface>) =
                    push(Event.TetheredInterfacesChanged(toInterfacesCompat(interfaces))).run { }
                @Keep
                fun onLocalOnlyInterfacesChanged(interfaces: Set<TetheringInterface>) =
                    push(Event.LocalOnlyInterfacesChanged(toInterfacesCompat(interfaces))).run { }
                @Keep
                fun onError(iface: TetheringInterface, error: Int) =
                    push(Event.ErrorChanged(toInterfaceCompat(iface), error)).run { }
            }
        } catch (e: NoClassDefFoundError) {
            if (Build.VERSION.SDK_INT >= 31) Timber.w(e)
            LegacyCallback()
        }
        Services.tethering.registerTetheringEventCallback(InPlaceExecutor, callback)
        return@binderCallbackFlow {
            try {
                Services.tethering.unregisterTetheringEventCallback(callback)
            } catch (e: IllegalStateException) {
                if (e.cause !is DeadObjectException) throw e
            }
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
    @Deprecated("Use TetheringEvent.ErrorChanged from tetheringEventFlow instead.")
    fun getLastTetherError(iface: String): Int = getLastTetherError(Services.connectivity, iface) as Int

    val tetherErrorLookup = ConstantLookup("TETHER_ERROR_",
            "NO_ERROR", "UNKNOWN_IFACE", "SERVICE_UNAVAIL", "UNSUPPORTED", "UNAVAIL_IFACE", "MASTER_ERROR",
            "TETHER_IFACE_ERROR", "UNTETHER_IFACE_ERROR", "ENABLE_NAT_ERROR", "DISABLE_NAT_ERROR",
            "IFACE_CFG_ERROR", "PROVISION_FAILED", "DHCPSERVER_ERROR", "ENTITLEMENT_UNKNOWN") @TargetApi(30) {
        TetheringManager::class.java
    }
}
