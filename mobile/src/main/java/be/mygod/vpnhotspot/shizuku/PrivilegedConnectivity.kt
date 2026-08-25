package be.mygod.vpnhotspot.shizuku

import android.content.Context
import android.content.ContextWrapper
import android.net.ConnectivityManager
import android.net.IConnectivityManager
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import java.lang.reflect.Modifier

/**
 * The privileged [ConnectivityManager] Shizuku mode needs, for exactly three operations: the exact
 * foreground request, its release, and the agent's `CONNECTIVITY_SERVICE` lookup.
 *
 * Every hidden [ConnectivityManager] constructor can assign the private static singleton, so none of
 * them is invoked: the copy is allocated without a constructor and every declared instance field is
 * inherited from the process's ordinary manager. Copying the whole field set instead of a
 * per-release minimum is deliberate, because field initializers run in the skipped constructor and
 * the field set is Mainline-dependent.
 *
 * Every declared instance field is assigned into the uninitialized copy; `mContext` and `mService` are then
 * the two that are overridden. Consequently every field except those two is aliased, not owned: both managers
 * mutate one set of collections and share one monitor. That is why this manager is restricted to the three
 * operations above; default-network activity listeners, the tethering shims and their event callbacks, QoS
 * callbacks, and every other API backed by that shared state are forbidden on it.
 *
 * The three operations are also not the only framework access points, which is what makes "restricted to
 * three" a containment claim rather than a call-site count. Constructing the custom `NetworkAgent` resolves
 * `CONNECTIVITY_SERVICE` through the private context and, on releases that have it, probes
 * `isFeatureEnabled` - served from the cache warmed and copied below, so it issues no wrapped transaction -
 * and `NetworkAgent.register` reaches `ConnectivityManager.registerNetworkAgent`. Both are inside the agent's
 * own publication, both authorize on UID alone, and neither touches the aliased collections.
 *
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/ConnectivityManager.java#2626
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#2944
 */
class PrivilegedConnectivity private constructor(
    val manager: ConnectivityManager,
    /**
     * Retained separately because the TestNetwork interface binder is handed out only by
     * `startOrGetTestNetworkService`, which enforces `MANAGE_TEST_NETWORKS` on the calling UID.
     * Routing that through [manager] instead would widen a manager whose collections are shared with
     * the ordinary one.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#14418
     */
    val service: IConnectivityManager,
    val context: Context,
) {
    /**
     * Shared by the privileged manager and the custom agent, and never returned from an app service,
     * singleton, or any other IPC surface.
     *
     * `getSystemServiceName` is deliberately not overridden, so the typed
     * `getSystemService(ConnectivityManager::class.java)` the agent performs resolves through the
     * string override below.
     */
    private class PrivilegedContext(base: Context, private val opPackage: String) : ContextWrapper(base) {
        /**
         * Assigned after construction: the manager is built with this context as its own `mContext`,
         * so neither can be complete before the other exists.
         */
        lateinit var manager: ConnectivityManager

        override fun getSystemService(name: String): Any? =
            if (name == Context.CONNECTIVITY_SERVICE) manager else super.getSystemService(name)
        override fun getOpPackageName() = opPackage
        override fun getAttributionTag(): String? = null
    }

    companion object {
        private val classUnsafe by lazy { Class.forName("sun.misc.Unsafe") }
        /**
         * `Unsafe.getUnsafe()` rejects app-classloader callers, so the singleton is reached through its field
         * instead. Both members are `unsupported` rather than `blocked`, and the shape is identical on
         * Android 13 and 17.
         *
         * If either member is unavailable, the session fails before any TUN, request, preference or agent
         * mutation.
         *
         * https://android.googlesource.com/platform/libcore/+/refs/tags/android-13.0.0_r1/ojluni/src/main/java/sun/misc/Unsafe.java#57
         * https://android.googlesource.com/platform/libcore/+/refs/tags/android-17.0.0_r1/ojluni/src/main/java/sun/misc/Unsafe.java#63
         */
        private val theUnsafe by lazy {
            classUnsafe.getDeclaredField("theUnsafe").apply { isAccessible = true }.get(null)
        }
        private val allocateInstance by lazy {
            classUnsafe.getDeclaredMethod("allocateInstance", Class::class.java)
        }

        /**
         * Issues no wrapped transaction: `asInterface` only builds a proxy, and the feature-cache
         * warming below runs on the ordinary manager's own binder. Any failure here is terminal for
         * the session, but it happens before any TUN, request, preference, or agent mutation, so
         * there is nothing to roll back and no poisoned process state.
         */
        fun create(epoch: ShizukuEpoch): PrivilegedConnectivity {
            val fieldContext = UnblockCentral.ConnectivityManager_mContext
            val fieldService = UnblockCentral.ConnectivityManager_mService
            val fieldInstance = UnblockCentral.ConnectivityManager_sInstance
            // the only correctly constructed instance in this process, so it is the template
            val ordinary = Services.connectivity
            val ordinaryService = fieldService.get(ordinary) as IConnectivityManager
            checkNotNull(fieldContext.get(ordinary)) { "Ordinary ConnectivityManager has no context" }
            // any feature argument fills the whole cache, so the copy inherits a populated value
            UnblockCentral.ConnectivityManager_isFeatureEnabled?.invoke(ordinary, 0L)
            val singleton = fieldInstance.get(null)
            val context = PrivilegedContext(app, epoch.opPackageName)
            val service = IConnectivityManager.Stub.asInterface(epoch.wrap(ordinaryService.asBinder()))
            val manager = allocateInstance(theUnsafe, ConnectivityManager::class.java) as ConnectivityManager
            for (field in ConnectivityManager::class.java.declaredFields) {
                if (Modifier.isStatic(field.modifiers)) continue
                field.isAccessible = true
                field.set(manager, field.get(ordinary))
            }
            fieldContext.set(manager, context)
            fieldService.set(manager, service)
            check(fieldContext.get(manager) === context) { "Privileged manager rejected its context" }
            check(fieldService.get(manager) === service) { "Privileged manager rejected its service" }
            check(fieldInstance.get(null) === singleton) { "ConnectivityManager singleton was written" }
            check(app.getSystemService(ConnectivityManager::class.java) === ordinary) {
                "Ordinary context stopped returning the ordinary manager"
            }
            context.manager = manager
            return PrivilegedConnectivity(manager, service, context)
        }
    }
}
