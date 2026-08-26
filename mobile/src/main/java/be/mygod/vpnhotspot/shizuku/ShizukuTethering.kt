package be.mygod.vpnhotspot.shizuku

import android.net.IIntResultListener
import android.net.ITetheringConnector
import android.net.TetheringManager
import android.os.Build
import android.os.IBinder
import android.os.RemoteException
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import be.mygod.vpnhotspot.util.callSuper
import be.mygod.vpnhotspot.util.findIdentifier
import be.mygod.vpnhotspot.util.matches
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.selects.select
import kotlinx.coroutines.suspendCancellableCoroutine
import timber.log.Timber
import java.lang.reflect.InvocationHandler
import java.lang.reflect.Method
import java.lang.reflect.Proxy
import kotlin.coroutines.resumeWithException

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
     * The tethering process is gone, and [died] says so. Raised wherever that ends something this app was
     * relying on it for: an answer it still owed, the first upstream observation a startup needs, or a
     * connector acquisition whose `linkToDeath` was refused.
     *
     * Deliberately not a [TetheringManagerCompat.Failure]: that carries a result code the service actually
     * returned, which is authoritative proof it did not act. This one proves something different and just as
     * definite - that the flag is gone with the process that held it - so the debt is discharged rather than
     * left unknown, and it is the session rather than the ledger that this is a failure for.
     */
    class DiedException(message: String) : Exception(message)

    /**
     * Completes [died] and nothing else. Whatever any one waiter concludes from that death is its own to
     * conclude; what this recipient owns is the process-terminal fact, recorded before anything can act on
     * it.
     */
    private val recipient = IBinder.DeathRecipient { died.complete(Unit) }

    init {
        binder.linkToDeath(recipient, 0)
    }

    /**
     * Moves the global preference and records the outcome in [debt] according to what the answer actually
     * proves - including the two answers that are not result codes: the tethering process dying, which proves
     * the flag is gone, and everything else, which proves nothing either way.
     *
     * Death is read from [died] rather than only from the wait below, because it does not only arrive as an
     * ending for that wait: the `oneway` send can itself throw against a binder that has already gone, and
     * classifying that as unknown would leave a debt the death already settled and send the withdrawal to
     * retry it through a connector that can never work. So the latch is the first question this asks on every
     * failure - ahead of the result code, which it outranks, because a code only proves what one transaction
     * did while the latch proves there is no longer anywhere for the flag to live - and a failure with no
     * observed death still leaves the flag unknown.
     *
     * Denial is silent: without `NETWORK_SETTINGS` the service reports
     * `TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION` through the listener instead of throwing, so the result
     * code is required rather than optional. The interface is `oneway`, so the transact returns without
     * waiting on the service and the result arrives separately - and exactly three things can end that
     * wait: the result, [died], or the owner cancelling the lifespan.
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
            // Ended by the service's own answer, by that service dying, or by the owner cancelling this
            // lifespan, and by nothing else. A service that is merely slow is waited out however long it
            // takes, because inventing an answer for it would record a state nobody observed. [died] is not
            // that kind of ending: it is the terminal fact that the listener this result would arrive
            // through lives in a process that has gone, so waiting on the result alone could never end -
            // and the ordered retirement issues the clear under `NonCancellable`, where a wait that never
            // ends is the child fence after it never running. Biased to the result, so an answer already
            // delivered still wins the turn it shares with the death that followed it.
            val code = select<Int> {
                result.onAwait { it }
                died.onAwait { throw DiedException(app.getString(R.string.shizuku_failure_tethering_died)) }
            }
            if (code != TetheringManager.TETHER_ERROR_NO_ERROR) {
                // Authoritative: the permission check runs before the mutation is posted, so a nonzero code
                // is the service saying it did not act rather than saying it failed halfway.
                if (prefer) debt.settingDenied() else debt.clearingDenied()
                throw TetheringManagerCompat.Failure(code)
            }
            epoch.ensureCurrent()
        } catch (e: Throwable) {
            // The latch is asked first, ahead of even the result code, because it is the only proof here
            // that outranks one. A nonzero code proves *this transaction* did not mutate, and that is what
            // restores the debt it came from - but a debt restored to `LIVE` is a flag in a process that has
            // gone, and there is nothing there to hold it any more.
            if (!died.isCompleted) {
                // Authoritative, and already recorded above, so nothing here reclassifies it.
                if (e is TetheringManagerCompat.Failure) throw e
                // Anything else - a replaced epoch, this caller going away - leaves the handler free to
                // have mutated, and no answer this app can act on. Including a dead-binder throw whose
                // death has not been observed yet: an incomplete latch is genuinely "not known", and an
                // unknown clear is idempotent and gets retried, while a wrong discharge cannot be undone.
                if (prefer) debt.settingUnknown() else debt.clearingUnknown()
                throw e
            }
            // Positive proof, and the one non-result answer that is: `mPreferTestNetworks` lives in the
            // process that has just gone and a restarted service begins from `false`, so whether this
            // transaction landed no longer changes what is owed - nothing is. Recording it unknown, or
            // restoring what a denial came from, would send the withdrawal's own release to retry a
            // discharged debt through `TetheringManager`'s permanently cached dead connector, which is
            // residue this app invented rather than state it left behind.
            //
            // Asked of the latch rather than only of the select's own terminal clause, because the same
            // death also arrives as a throw out of the `oneway` send itself - and then the failure that
            // reaches this catch is a dead binder's, or even a result code's, rather than [DiedException].
            // The proof is the latch either way, and it is somebody else's: it says nothing about *this*
            // failure's shape, only that the flag is gone. So this never reads a Shizuku, epoch or service
            // failure as network-stack death; it reads a death that was independently observed.
            debt.lostWithService()
            // A cancellation is this caller going away and stays one, whatever the latch says.
            if (e is DiedException || e is CancellationException) throw e
            // Death settles this app's own debt; it settles nothing about the JVM. An `OutOfMemoryError`, a
            // `LinkageError` or a failed assertion is not an operational outcome to be reported as one - and
            // reported as one it would arrive at the withdrawal's own boundary looking like the expected
            // failure there, and be consumed.
            if (e !is Exception) throw e
            // Otherwise reported as the proof it is, so one [DiedException] handler is enough for every
            // caller of this however the death reached it, with the original failure travelling along.
            throw DiedException(app.getString(R.string.shizuku_failure_tethering_died)).apply {
                addSuppressed(e)
            }
        }
        if (prefer) debt.settingMutated() else debt.clearingConfirmed()
    }

    /**
     * Drops this session's death recipient, and takes the answer seriously rather than discarding it.
     *
     * `false` means the target has already died and its recipient "has been (or soon will be) called". In the
     * second half of that the notification is still in flight, against a connector the ledger is about to
     * confirm and drop - so nothing would have completed [died], and a successor could reach the startup gate
     * and pass it before the callback landed. Latching the same fact here closes that window synchronously,
     * which is the whole reason the Boolean is read: it is the one place death is knowable without waiting
     * for the callback that reports it.
     *
     * `NoSuchElementException` is deliberately not handled: it is documented for a recipient that was never
     * registered while the binder is still alive, and the ledger already admits exactly one unlink per
     * connector - construction linked this recipient or threw, and [ConnectorResource] hands the connector
     * out once.
     *
     * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-13.0.0_r1/core/java/android/os/IBinder.java#351
     * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-17.0.0_r1/core/java/android/os/IBinder.java#375
     */
    fun unlink() {
        if (!binder.unlinkToDeath(recipient, 0)) died.complete(Unit)
    }

    companion object {
        /**
         * The tethering process every connector in this app process speaks to has died, latched for the life
         * of the app process.
         *
         * One fact rather than one per connector, because there is only one binder here to die:
         * `TetheringManager` caches its connector permanently and AOSP states that after a network stack
         * crash "no recovery is possible", so recovery needs a new app process rather than a new session, and
         * a further session is refused with that in so many words rather than left to fail obscurely at
         * connector acquisition. Completed directly by all three ways this app can know that death without
         * waiting: a death recipient firing, [acquire] being handed a binder `linkToDeath` refuses, and
         * [unlink] being told the target is already dead. Whichever happens first records it, with no
         * session, watcher or result selection in between and nothing to clear it again - which is what lets
         * every reader treat "known" and "settled" as the same thing.
         *
         * Nothing else surfaces network-stack death: [TetheringManagerCompat.eventFlow] forwards callback
         * events and installs no death recipient, so without this a session would stay `ACTIVE` across a
         * crash that has already reset the preference and reselected an ordinary upstream.
         *
         * Raced by [setPreferTestNetworks], by the first upstream observation a startup waits for, and by a
         * committed session's own ending watcher. Each of those waits on something only that process can
         * produce, so this is what ends them when no answer can come and no cancellation is coming either -
         * which for the clear is exactly the ordered retirement, run under `NonCancellable` and with the
         * child fence behind it. What a session's ledger concludes from the same death is separate and
         * narrower, because it is a fact about one generation's debt rather than about the process:
         * [PreferenceResource.lostWithService].
         *
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#467
         */
        val died = CompletableDeferred<Unit>()

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
         *
         * The delivery below can land on either of two AOSP paths, and only one of them carries a throw back
         * to this caller. With a connector already cached the consumer runs inline and an exception reaches
         * here; before then the consumer is held in a wait queue that `onTetheringConnected` drains later, and
         * that drain catches `RemoteException` per task, logs it and moves on to the next. So a failure raised
         * in the callback on the queued path resumes nothing at all, which is why the one expected failure
         * there is caught where it happens rather than allowed to escape.
         *
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#365
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#480
         */
        suspend fun acquire(epoch: ShizukuEpoch) = suspendCancellableCoroutine { cont ->
            // Resolved here rather than inside the callback, and for the same reason [ShizukuTestNetwork]
            // resolves its own members up front: a reflective failure raised inside AOSP's queue drain is
            // neither answered nor survivable, because that drain expects only `RemoteException`.
            UnblockCentral.ITetheringConnector_asInterface
            val handler = object : InvocationHandler {
                override fun invoke(proxy: Any, method: Method, args: Array<out Any?>?) = when {
                    method.matches("onConnectorAvailable", ITetheringConnector::class.java) -> {
                        val binder = (args!![0] as ITetheringConnector).asBinder()
                        try {
                            // a connector delivered after cancellation still has a linked recipient
                            cont.resume(PinnedTetheringConnector(epoch, binder,
                                UnblockCentral.ITetheringConnector_asInterface(null, epoch.wrap(binder))
                                        as ITetheringConnector)) { _, value, _ -> value.unlink() }
                        } catch (e: RemoteException) {
                            // `linkToDeath`'s one documented failure: the target's process has already died.
                            // The queued delivery path makes that an ordinary case rather than a race - AOSP
                            // caches its connector permanently, so a queue drained after the network stack
                            // crashed hands out a binder that is already gone - and it is the process-terminal
                            // fact rather than this acquisition's own bad luck, so it is latched before
                            // anything is told about it. Then answered as this call's failure, because the
                            // alternative on that path is not a thrown exception but a suspension nothing
                            // will ever resume. A continuation already cancelled absorbs this, and the latch
                            // above is what a later start reads instead.
                            died.complete(Unit)
                            cont.resumeWithException(DiedException(
                                app.getString(R.string.shizuku_failure_tethering_died)).apply {
                                    addSuppressed(e)
                                })
                        }
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
