package be.mygod.vpnhotspot.net

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.Network
import android.net.TetheringManager
import android.os.Build
import android.os.Parcelable
import androidx.annotation.MainThread
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetheringManagerCompat.availableIfaces
import be.mygod.vpnhotspot.net.TetheringManagerCompat.localOnlyTetheredIfaces
import be.mygod.vpnhotspot.net.TetheringManagerCompat.tetheredIfaces
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.ensureReceiverUnregistered
import timber.log.Timber

/**
 * Convenience class that reassembles TetherStatesParcel from TetheringEventCallback.
 */
data class TetherStates(
    val available: Set<String> = emptySet(),
    val tethered: Set<String> = emptySet(),
    val localOnly: Set<String> = emptySet(),
    val errored: Map<String, Int> = emptyMap(),
) {
    interface Callback : TetheringManagerCompat.TetheringEventCallback {
        fun onTetherStatesChanged(states: TetherStates) {}
    }

    companion object : BroadcastReceiver(), Runnable, TetheringManagerCompat.TetheringEventCallback {
        private val callbacks = linkedSetOf<Callback>()
        private var states = TetherStates()
        private var dispatchPending = false
        private var publicCallbackRegistered = false
        private var broadcastReceiverRegistered = false

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
            callbacks.toList().forEach { it.onTetherStatesChanged(states) }
        }

        @MainThread
        private fun scheduleDispatch() {
            if (dispatchPending) return
            dispatchPending = true
            Services.mainHandler.post(this)
        }

        @MainThread
        override fun onReceive(context: Context?, intent: Intent) {
            states = TetherStates(
                intent.availableIfaces?.toSet() ?: return,
                intent.tetheredIfaces?.toSet() ?: return,
                intent.localOnlyTetheredIfaces?.toSet() ?: return,
                intent.getStringArrayListExtra(TetheringManagerCompat.EXTRA_ERRORED_TETHER)
                    ?.associateWith { states.errored[it] ?: 0 } ?: return,
            )
            run()
        }

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

        override fun onTetheringSupported(supported: Boolean) =
            callbacks.toList().forEach { it.onTetheringSupported(supported) }

        override fun onSupportedTetheringTypes(supportedTypes: Set<Int?>) =
            callbacks.toList().forEach { it.onSupportedTetheringTypes(supportedTypes) }

        override fun onUpstreamChanged(network: Network?) =
            callbacks.toList().forEach { it.onUpstreamChanged(network) }

        override fun onTetherableInterfaceRegexpsChanged(reg: Any?) =
            callbacks.toList().forEach { it.onTetherableInterfaceRegexpsChanged(reg) }

        @MainThread
        override fun onError(ifName: String, error: Int) {
            callbacks.toList().forEach { it.onError(ifName, error) }
            states = states.copy(
                errored = if (error == 0) {
                    states.errored - ifName
                } else {
                    states.errored + (ifName to error)
                },
            )
            scheduleDispatch()
        }

        @MainThread
        override fun onTetherableInterfacesChanged(interfaces: List<String?>) {
            val available = interfaces.filterNotNull().toSet()
            callbacks.toList().forEach { it.onTetherableInterfacesChanged(interfaces) }
            states = states.copy(
                available = available,
                errored = states.errored.filterKeys { it !in available },
            )
            scheduleDispatch()
        }

        @MainThread
        override fun onTetheredInterfacesChanged(interfaces: List<String?>) {
            val tethered = interfaces.filterNotNull().toSet()
            callbacks.toList().forEach { it.onTetheredInterfacesChanged(interfaces) }
            states = states.copy(
                tethered = tethered,
                errored = states.errored.filterKeys { it !in tethered },
            )
            scheduleDispatch()
        }

        @MainThread
        override fun onLocalOnlyInterfacesChanged(interfaces: List<String?>) {
            val localOnly = interfaces.filterNotNull().toSet()
            callbacks.toList().forEach { it.onLocalOnlyInterfacesChanged(interfaces) }
            states = states.copy(
                localOnly = localOnly,
                errored = states.errored.filterKeys { it !in localOnly },
            )
            scheduleDispatch()
        }

        override fun onClientsChanged(clients: Collection<Parcelable>) =
            callbacks.toList().forEach { it.onClientsChanged(clients) }

        override fun onOffloadStatusChanged(status: Int) =
            callbacks.toList().forEach { it.onOffloadStatusChanged(status) }

        @MainThread
        fun registerCallback(callback: Callback) {
            if (!callbacks.add(callback) || callbacks.size > 1) return
            if (Build.VERSION.SDK_INT < 30) {
                app.registerReceiver(this, IntentFilter(TetheringManagerCompat.ACTION_TETHER_STATE_CHANGED))
                broadcastReceiverRegistered = true
                return
            }
            try {
                TetheringManagerCompat.registerTetheringEventCallback(this, app.mainExecutor)
                publicCallbackRegistered = true
            } catch (e: Exception) {
                Timber.w(e)
                app.registerReceiver(this,
                    IntentFilter(TetheringManagerCompat.ACTION_TETHER_STATE_CHANGED))
                broadcastReceiverRegistered = true
                return
            }
            if (broadcastReceiverRegistered || Build.VERSION.SDK_INT >= 31 ||
                publicLocalOnlyInterfacesChangedSupported) return
            app.registerReceiver(this, IntentFilter(TetheringManagerCompat.ACTION_TETHER_STATE_CHANGED))
            broadcastReceiverRegistered = true
        }

        @MainThread
        fun unregisterCallback(callback: Callback) {
            if (!callbacks.remove(callback) || callbacks.isNotEmpty()) return
            if (publicCallbackRegistered) {
                TetheringManagerCompat.unregisterTetheringEventCallback(this)
                publicCallbackRegistered = false
            }
            if (broadcastReceiverRegistered) {
                app.ensureReceiverUnregistered(this)
                broadcastReceiverRegistered = false
            }
            Services.mainHandler.removeCallbacks(this)
            dispatchPending = false
            states = TetherStates()
        }
    }
}
