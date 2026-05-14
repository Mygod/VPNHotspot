package be.mygod.vpnhotspot.net

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.Network
import android.net.TetheringManager
import android.os.Build
import android.os.Parcelable
import androidx.collection.MutableScatterMap
import androidx.annotation.MainThread
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.ensureReceiverUnregistered
import kotlinx.collections.immutable.PersistentMap
import kotlinx.collections.immutable.PersistentSet
import kotlinx.collections.immutable.persistentMapOf
import kotlinx.collections.immutable.persistentSetOf

/**
 * Convenience class that reassembles TetherStatesParcel from TetheringEventCallback + backfilling compat for API 30-.
 */
data class TetherStates(
    val available: PersistentSet<String> = persistentSetOf(),
    val tethered: PersistentSet<String> = persistentSetOf(),
    val localOnly: PersistentSet<String> = persistentSetOf(),
    val errored: PersistentMap<String, Int> = persistentMapOf(),
) {
    interface Callback : TetheringManagerCompat.TetheringEventCallback {
        fun onTetherStatesChanged(states: TetherStates) {}
    }

    private class Registration(private val callback: Callback) : BroadcastReceiver(), Runnable,
        TetheringManagerCompat.TetheringEventCallback {
        private var states = TetherStates()
        private var dispatchPending = false
        private var fallbackToBroadcast = false

        /**
         * On runtimes that expose public `onLocalOnlyInterfacesChanged`, AOSP dispatches startup
         * tether-state callbacks from one `executor.execute { ... }` block in `onCallbackStarted`,
         * and later tether-state updates from one `executor.execute { ... }` block in
         * `onTetherStatesChanged`. Coalescing onto the main handler turns each burst into one
         * `onTetherStatesChanged`.
         *
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-12.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#1262
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-12.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#1296
         */
        override fun run() {
            dispatchPending = false
            callback.onTetherStatesChanged(states)
        }

        @MainThread
        private fun scheduleDispatch() {
            if (dispatchPending) return
            dispatchPending = true
            Services.mainHandler.post(this)
        }

        /**
         * Broadcast-backed registrations parse `ACTION_TETHER_STATE_CHANGED` extras directly.
         * android-10.0.0_r1 `ConnectivityManager` uses `"availableArray"`, `"tetherArray"`, and
         * `"localOnlyArray"`/`"erroredArray"`, while android-11.0.0_r1 `TetheringManager` keeps
         * `"availableArray"`/`"tetherArray"`/`"erroredArray"` and changes local-only to
         * `"android.net.extra.ACTIVE_LOCAL_ONLY"`. The action string remains
         * `"android.net.conn.TETHER_STATE_CHANGED"`.
         *
         * https://android.googlesource.com/platform/frameworks/base/+/android-10.0.0_r1/core/java/android/net/ConnectivityManager.java#365
         * https://android.googlesource.com/platform/frameworks/base/+/android-10.0.0_r1/core/java/android/net/ConnectivityManager.java#370
         * https://android.googlesource.com/platform/frameworks/base/+/android-10.0.0_r1/core/java/android/net/ConnectivityManager.java#385
         * https://android.googlesource.com/platform/frameworks/base/+/android-10.0.0_r1/core/java/android/net/ConnectivityManager.java#393
         * https://android.googlesource.com/platform/frameworks/base/+/android-10.0.0_r1/core/java/android/net/ConnectivityManager.java#401
         * https://android.googlesource.com/platform/frameworks/base/+/android-11.0.0_r1/packages/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#94
         * https://android.googlesource.com/platform/frameworks/base/+/android-11.0.0_r1/packages/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#97
         * https://android.googlesource.com/platform/frameworks/base/+/android-11.0.0_r1/packages/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#107
         * https://android.googlesource.com/platform/frameworks/base/+/android-11.0.0_r1/packages/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#115
         * https://android.googlesource.com/platform/frameworks/base/+/android-11.0.0_r1/packages/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#123
         */
        @MainThread
        override fun onReceive(context: Context?, intent: Intent) {
            val available = intent.getStringArrayListExtra("availableArray") ?: return
            val tethered = intent.getStringArrayListExtra("tetherArray") ?: return
            val localOnly = intent.getStringArrayListExtra(
                if (Build.VERSION.SDK_INT >= 30) "android.net.extra.ACTIVE_LOCAL_ONLY" else
                    "localOnlyArray") ?: return
            val errored = intent.getStringArrayListExtra("erroredArray") ?: return
            val nextErrored = persistentMapOf<String, Int>().builder()
            for (iface in errored) nextErrored[iface] = states.errored[iface] ?: 0
            states = TetherStates(available.toPersistentIfaceSet(), tethered.toPersistentIfaceSet(),
                localOnly.toPersistentIfaceSet(), nextErrored.build())
            if (fallbackToBroadcast) {
                states.errored.forEach { (iface, error) -> callback.onError(iface, error) }
                callback.onTetherableInterfacesChanged(available)
                callback.onTetheredInterfacesChanged(tethered)
            }
            callback.onLocalOnlyInterfacesChanged(localOnly)
            scheduleDispatch()
        }

        override fun onTetheringSupported(supported: Boolean) = callback.onTetheringSupported(supported)
        override fun onSupportedTetheringTypes(supportedTypes: Set<Int?>) =
            callback.onSupportedTetheringTypes(supportedTypes)
        override fun onUpstreamChanged(network: Network?) = callback.onUpstreamChanged(network)
        override fun onTetherableInterfaceRegexpsChanged(reg: Any?) =
            callback.onTetherableInterfaceRegexpsChanged(reg)

        @MainThread
        override fun onError(ifName: String, error: Int) {
            callback.onError(ifName, error)
            states = states.copy(
                errored = if (error == 0) {
                    states.errored.remove(ifName)
                } else {
                    states.errored.put(ifName, error)
                },
            )
            scheduleDispatch()
        }

        @MainThread
        override fun onTetherableInterfacesChanged(interfaces: List<String?>) {
            val available = interfaces.toPersistentIfaceSet()
            callback.onTetherableInterfacesChanged(interfaces)
            states = states.copy(
                available = available,
                errored = states.errored.withoutKeysIn(available),
            )
            scheduleDispatch()
        }

        @MainThread
        override fun onTetheredInterfacesChanged(interfaces: List<String?>) {
            val tethered = interfaces.toPersistentIfaceSet()
            callback.onTetheredInterfacesChanged(interfaces)
            states = states.copy(
                tethered = tethered,
                errored = states.errored.withoutKeysIn(tethered),
            )
            scheduleDispatch()
        }

        @MainThread
        override fun onLocalOnlyInterfacesChanged(interfaces: List<String?>) {
            val localOnly = interfaces.toPersistentIfaceSet()
            callback.onLocalOnlyInterfacesChanged(interfaces)
            states = states.copy(
                localOnly = localOnly,
                errored = states.errored.withoutKeysIn(localOnly),
            )
            scheduleDispatch()
        }

        override fun onClientsChanged(clients: Collection<Parcelable>) = callback.onClientsChanged(clients)
        override fun onOffloadStatusChanged(status: Int) = callback.onOffloadStatusChanged(status)

        @MainThread
        fun register() {
            if (Build.VERSION.SDK_INT < 30) {
                fallbackToBroadcast = true
                app.registerReceiver(this, IntentFilter(ACTION_TETHER_STATE_CHANGED))
                return
            }
            TetheringManagerCompat.registerTetheringEventCallback(this, app.mainExecutor)
            if (Build.VERSION.SDK_INT >= 31 || publicLocalOnlyInterfacesChangedSupported) return
            app.registerReceiver(this, IntentFilter(ACTION_TETHER_STATE_CHANGED))
        }

        @MainThread
        fun unregister() {
            if (Build.VERSION.SDK_INT >= 30 && !fallbackToBroadcast) {
                TetheringManagerCompat.unregisterTetheringEventCallback(this)
            }
            if (fallbackToBroadcast || Build.VERSION.SDK_INT == 30 && !publicLocalOnlyInterfacesChangedSupported) {
                app.ensureReceiverUnregistered(this)
            }
            Services.mainHandler.removeCallbacks(this)
        }
    }

    companion object {
        private const val ACTION_TETHER_STATE_CHANGED = "android.net.conn.TETHER_STATE_CHANGED"
        private val registrations = MutableScatterMap<Callback, Registration>()

        /**
         * android-11.0.0_r1 public `TetheringEventCallback` does not expose local-only interfaces,
         * while android-12.0.0_r1 adds `onLocalOnlyInterfacesChanged`.
         *
         * https://android.googlesource.com/platform/frameworks/base/+/android-11.0.0_r1/packages/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#916
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-12.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#1141
         */
        @get:RequiresApi(30)
        private val publicLocalOnlyInterfacesChangedSupported by lazy {
            TetheringManager.TetheringEventCallback::class.java.methods
                .any { it.name == "onLocalOnlyInterfacesChanged" }
        }

        @MainThread
        fun registerCallback(callback: Callback) {
            registrations.compute(callback) { _, registration ->
                registration ?: Registration(callback).apply { register() }
            }
        }

        @MainThread
        fun unregisterCallback(callback: Callback) = registrations.remove(callback)?.unregister()

        private fun Iterable<String?>.toPersistentIfaceSet(): PersistentSet<String> {
            val builder = persistentSetOf<String>().builder()
            for (iface in this) if (iface != null) builder.add(iface)
            return builder.build()
        }

        private fun PersistentMap<String, Int>.withoutKeysIn(removed: PersistentSet<String>): PersistentMap<String, Int> {
            if (isEmpty() || removed.isEmpty()) return this
            var mapBuilder: PersistentMap.Builder<String, Int>? = null
            for (iface in keys) if (iface in removed) {
                (mapBuilder ?: builder().also { mapBuilder = it }).remove(iface)
            }
            return mapBuilder?.build() ?: this
        }
    }
}
