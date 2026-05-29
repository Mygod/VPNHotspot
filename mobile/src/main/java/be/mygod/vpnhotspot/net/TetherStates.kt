package be.mygod.vpnhotspot.net

import android.content.IntentFilter
import android.net.TetheringManager
import android.os.Build
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.ensureReceiverUnregistered
import kotlinx.collections.immutable.PersistentMap
import kotlinx.collections.immutable.PersistentSet
import kotlinx.collections.immutable.persistentMapOf
import kotlinx.collections.immutable.persistentSetOf
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.channelFlow
import kotlinx.coroutines.launch

/**
 * Convenience class that reassembles TetherStates from [TetheringManagerCompat.eventFlow], backfilling
 * compat for API 30-.
 */
data class TetherStates(
    val available: PersistentSet<String> = persistentSetOf(),
    val tethered: PersistentSet<String> = persistentSetOf(),
    val localOnly: PersistentSet<String> = persistentSetOf(),
    val errored: PersistentMap<String, Int> = persistentMapOf(),
) {
    companion object {
        private const val ACTION_TETHER_STATE_CHANGED = "android.net.conn.TETHER_STATE_CHANGED"

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

        /**
         * The reassembled tether states as a cold flow — one source registration per collector. API 30+
         * folds [TetheringManagerCompat.eventFlow]; API 30- (and API 30 runtimes lacking public
         * `onLocalOnlyInterfacesChanged`) parse the sticky `ACTION_TETHER_STATE_CHANGED` broadcast,
         * which carries local-only membership the public callback omits there.
         *
         * AOSP dispatches its startup callback burst from one `executor.execute { ... }` block, so the
         * folds are coalesced onto the main handler into a single emission per burst.
         *
         * Broadcast extras differ by release: android-10 `ConnectivityManager` used `"localOnlyArray"`,
         * android-11 `TetheringManager` renamed local-only to `"android.net.extra.ACTIVE_LOCAL_ONLY"`.
         */
        val flow: Flow<TetherStates> = channelFlow {
            var states = TetherStates()
            var dispatchPending = false
            val dispatch = Runnable {
                dispatchPending = false
                trySend(states)
            }
            fun scheduleDispatch() {
                if (dispatchPending) return
                dispatchPending = true
                Services.mainHandler.post(dispatch)
            }
            var broadcastRegistered = false
            val receiver = broadcastReceiver { _, intent ->
                val available = intent.getStringArrayListExtra("availableArray") ?: return@broadcastReceiver
                val tethered = intent.getStringArrayListExtra("tetherArray") ?: return@broadcastReceiver
                val localOnly = intent.getStringArrayListExtra(
                    if (Build.VERSION.SDK_INT >= 30) "android.net.extra.ACTIVE_LOCAL_ONLY" else
                        "localOnlyArray") ?: return@broadcastReceiver
                val errored = intent.getStringArrayListExtra("erroredArray") ?: return@broadcastReceiver
                val nextErrored = persistentMapOf<String, Int>().builder()
                for (iface in errored) nextErrored[iface] = states.errored[iface] ?: 0
                states = TetherStates(available.toPersistentIfaceSet(), tethered.toPersistentIfaceSet(),
                    localOnly.toPersistentIfaceSet(), nextErrored.build())
                scheduleDispatch()
            }
            if (Build.VERSION.SDK_INT < 30) {
                app.registerReceiver(receiver, IntentFilter(ACTION_TETHER_STATE_CHANGED))
                broadcastRegistered = true
            } else {
                launch(Dispatchers.Main.immediate) {
                    TetheringManagerCompat.eventFlow.collect { event ->
                        when (event) {
                            is TetheringManagerCompat.Event.ErrorChanged -> {
                                states = states.copy(errored = if (event.error == 0) {
                                    states.errored.remove(event.ifName)
                                } else {
                                    states.errored.put(event.ifName, event.error)
                                })
                                scheduleDispatch()
                            }
                            is TetheringManagerCompat.Event.TetherableInterfacesChanged -> {
                                val available = event.interfaces.toPersistentIfaceSet()
                                states = states.copy(available = available,
                                    errored = states.errored.withoutKeysIn(available))
                                scheduleDispatch()
                            }
                            is TetheringManagerCompat.Event.TetheredInterfacesChanged -> {
                                val tethered = event.interfaces.toPersistentIfaceSet()
                                states = states.copy(tethered = tethered,
                                    errored = states.errored.withoutKeysIn(tethered))
                                scheduleDispatch()
                            }
                            is TetheringManagerCompat.Event.LocalOnlyInterfacesChanged -> {
                                val localOnly = event.interfaces.toPersistentIfaceSet()
                                states = states.copy(localOnly = localOnly,
                                    errored = states.errored.withoutKeysIn(localOnly))
                                scheduleDispatch()
                            }
                            else -> { }     // offload/supported/upstream/regexps/clients aren't tether states
                        }
                    }
                }
                if (Build.VERSION.SDK_INT < 31 && !publicLocalOnlyInterfacesChangedSupported) {
                    app.registerReceiver(receiver, IntentFilter(ACTION_TETHER_STATE_CHANGED))
                    broadcastRegistered = true
                }
            }
            awaitClose {
                Services.mainHandler.removeCallbacks(dispatch)
                if (broadcastRegistered) app.ensureReceiverUnregistered(receiver)
            }
        }.buffer(Channel.UNLIMITED)

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
