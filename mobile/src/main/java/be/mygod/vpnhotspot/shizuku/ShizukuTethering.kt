package be.mygod.vpnhotspot.shizuku

import android.net.IIntResultListener
import android.net.ITetheringConnector
import android.net.TetheringManager
import android.os.Build
import android.os.IBinder
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import be.mygod.vpnhotspot.util.callSuper
import be.mygod.vpnhotspot.util.findIdentifier
import be.mygod.vpnhotspot.util.matches
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withTimeout
import timber.log.Timber
import java.lang.reflect.InvocationHandler
import java.lang.reflect.Method
import java.lang.reflect.Proxy

/**
 * The tethering preference, which is the one piece of system state this mode mutates that can outlive
 * the session. It belongs to tethering rather than to connectivity, so it is driven through a pinned
 * [ITetheringConnector] rather than through [TetheringManager.setPreferTestNetworks], which discards
 * the result code and blocks the caller for up to its own 60-second timeout.
 *
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#2241
 */
class PinnedTetheringConnector private constructor(
    private val epoch: ShizukuEpoch,
    /**
     * The unwrapped binder, which is what death is linked on: the Shizuku wrapper forwards
     * `linkToDeath` to the original anyway, and this death is the tethering service's, not Shizuku's.
     */
    private val binder: IBinder,
    private val connector: ITetheringConnector,
) {
    class UnsupportedDeviceException(message: String) : Exception(message)

    /**
     * Nothing else surfaces network-stack death: [TetheringManagerCompat.eventFlow] forwards callback
     * events and installs no death recipient, so without this the session would stay `ACTIVE` across a
     * crash that has already reset the preference and reselected an ordinary upstream.
     *
     * Recovery needs a new app process rather than a new session, because `TetheringManager` caches its
     * connector permanently and AOSP states that after a network stack crash "no recovery is possible".
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#467
     */
    val died = CompletableDeferred<Unit>()
    private val recipient = IBinder.DeathRecipient { died.complete(Unit) }

    init {
        binder.linkToDeath(recipient, 0)
    }

    /**
     * Moves the global preference and records the outcome in [debt] according to what the answer actually
     * proves.
     *
     * Denial is silent: without `NETWORK_SETTINGS` the service reports
     * `TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION` through the listener instead of throwing, so the result
     * code is required rather than optional. The interface is `oneway`, so the transact returns without
     * waiting on the service and the result arrives separately.
     *
     * Deliberately not [ShizukuEpoch.bracket]: that helper closes its check immediately after the `oneway`
     * send, which here is before the mutation has even been posted. The epoch is therefore checked at the
     * two moments that mean something - once before issuing, and once when a `TETHER_ERROR_NO_ERROR` could
     * confirm a mutation that has already happened.
     *
     * Success only means the global preference moved. It does not force upstream reselection, and it is
     * never proof that tethering selected this network.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/src/com/android/networkstack/tethering/TetheringService.java#274
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/TetheringService.java#286
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/Tethering.java#3109
     */
    internal suspend fun setPreferTestNetworks(prefer: Boolean, debt: PreferenceResource) {
        val result = CompletableDeferred<Int>()
        val listener = object : IIntResultListener.Stub() {
            override fun onResult(resultCode: Int) {
                result.complete(resultCode).run { }
            }
        }
        epoch.ensureCurrent()
        // Recorded before the IPC, because the transaction is the moment the preference may move and a
        // caller that recorded afterwards would have a window in which the flag is set and nothing owns it.
        if (prefer) debt.settingIssued() else debt.clearingIssued()
        try {
            connector.setPreferTestNetworks(prefer, listener)
            val code = withTimeout(CONTROL_RESULT_DEADLINE) { result.await() }
            if (code != TetheringManager.TETHER_ERROR_NO_ERROR) {
                // Authoritative: the permission check runs before the mutation is posted, so a nonzero code
                // is the service saying it did not act rather than saying it failed halfway.
                if (prefer) debt.settingDenied() else debt.clearingDenied()
                throw TetheringManagerCompat.Failure(code)
            }
            epoch.ensureCurrent()
        } catch (e: Throwable) {
            if (e !is TetheringManagerCompat.Failure) {
                // Anything else - the deadline, a replaced epoch, this caller going away - leaves the
                // handler free to have mutated, and no answer this app can act on.
                if (prefer) debt.settingUnknown(e) else debt.clearingUnknown(e)
            }
            throw e
        }
        if (prefer) debt.settingMutated() else debt.clearingConfirmed()
    }

    fun unlink() {
        binder.unlinkToDeath(recipient, 0)
    }

    companion object {
        /**
         * `chooseUpstreamType` consults the test-network preference only when automatic upstream
         * selection is on, and falls back to a configured type list otherwise, so on a build without it
         * the preference is never read and cycling tethering cannot help. Detect that rather than
         * leaving the user in a permanent `RESTART_REQUIRED` no action can clear.
         *
         * This is only reachable on Android 13. From Android 14 the tethering module forces automatic
         * mode on regardless of the resource, so there is nothing to check.
         *
         * A resource we cannot read is a warning rather than a refusal: AOSP's own fallback for a
         * missing resource is `false`, but an unreadable one is indistinguishable from a wrong package
         * lookup, and guessing wrong here would refuse to start on a device that works. The failure
         * mode if that guess is wrong is a visible `RESTART_REQUIRED`, not silent breakage.
         *
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/src/com/android/networkstack/tethering/Tethering.java#1798
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/TetheringConfiguration.java#219
         */
        fun requireAutomaticUpstream() {
            if (Build.VERSION.SDK_INT >= 34) return
            val info = TetheringManagerCompat.resolvedService.serviceInfo
            val resources = app.packageManager.getResourcesForApplication(info.applicationInfo)
            val id = resources.findIdentifier("config_tether_upstream_automatic", "bool",
                "com.android.networkstack.tethering", info.packageName)
            if (id == 0) return Timber.w(Exception("config_tether_upstream_automatic not found"))
            if (!resources.getBoolean(id)) throw UnsupportedDeviceException(
                "This device does not use automatic tethering upstream selection")
        }

        /**
         * The connector itself is acquired app-side and needs no privilege; only its transactions do, so
         * it is wrapped afterwards. Reuses the acquisition shape
         * [TetheringManagerCompat.stopTethering] already relies on.
         */
        suspend fun acquire(epoch: ShizukuEpoch) = withTimeout(CONTROL_RESULT_DEADLINE) { acquireBlocking(epoch) }

        private suspend fun acquireBlocking(epoch: ShizukuEpoch): PinnedTetheringConnector =
                suspendCancellableCoroutine { cont ->
            val handler = object : InvocationHandler {
                override fun invoke(proxy: Any, method: Method, args: Array<out Any?>?) = when {
                    method.matches("onConnectorAvailable", ITetheringConnector::class.java) -> {
                        val binder = (args!![0] as ITetheringConnector).asBinder()
                        // a connector delivered after cancellation still has a linked recipient
                        cont.resume(PinnedTetheringConnector(epoch, binder,
                            UnblockCentral.ITetheringConnector_asInterface(null, epoch.wrap(binder))
                                    as ITetheringConnector)) { _, value, _ -> value.unlink() }
                    }
                    else -> callSuper(UnblockCentral.TetheringManager_ConnectorConsumer, proxy, method, args)
                }
            }
            UnblockCentral.TetheringManager_getConnector(Services.tethering, Proxy.newProxyInstance(
                UnblockCentral.TetheringManager_ConnectorConsumer.classLoader,
                arrayOf(UnblockCentral.TetheringManager_ConnectorConsumer), handler))
        }
    }
}
