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
            callback.onTetherableInterfacesChanged(interfaces)
            states = states.copy(
                available = available,
                errored = states.errored.filterKeys { it !in available },
            )
            scheduleDispatch()
        }

        @MainThread
        override fun onTetheredInterfacesChanged(interfaces: List<String?>) {
            val tethered = interfaces.filterNotNull().toSet()
            callback.onTetheredInterfacesChanged(interfaces)
            states = states.copy(
                tethered = tethered,
                errored = states.errored.filterKeys { it !in tethered },
            )
            scheduleDispatch()
        }

        @MainThread
        override fun onLocalOnlyInterfacesChanged(interfaces: List<String?>) {
            val localOnly = interfaces.filterNotNull().toSet()
            callback.onLocalOnlyInterfacesChanged(interfaces)
            states = states.copy(
                localOnly = localOnly,
                errored = states.errored.filterKeys { it !in localOnly },
            )
            scheduleDispatch()
        }

        override fun onClientsChanged(clients: Collection<Parcelable>) = callback.onClientsChanged(clients)
        override fun onOffloadStatusChanged(status: Int) = callback.onOffloadStatusChanged(status)

        @MainThread
        fun register() {
            if (Build.VERSION.SDK_INT < 30) {
                fallbackToBroadcast = true
                app.registerReceiver(this, IntentFilter(TetheringManagerCompat.ACTION_TETHER_STATE_CHANGED))
                return
            }
            try {
                TetheringManagerCompat.registerTetheringEventCallback(this, app.mainExecutor)
            } catch (e: Exception) {
                Timber.w(e)
                fallbackToBroadcast = true
                app.registerReceiver(this, IntentFilter(TetheringManagerCompat.ACTION_TETHER_STATE_CHANGED))
                return
            }
            if (Build.VERSION.SDK_INT >= 31 || publicLocalOnlyInterfacesChangedSupported) return
            app.registerReceiver(this, IntentFilter(TetheringManagerCompat.ACTION_TETHER_STATE_CHANGED))
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
        private val registrations = mutableMapOf<Callback, Registration>()

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
            registrations.computeIfAbsent(callback) {
                Registration(it).apply { register() }
            }
        }

        @MainThread
        fun unregisterCallback(callback: Callback) = registrations.remove(callback)?.unregister()
    }
}
