package be.mygod.vpnhotspot

import android.app.Service
import android.content.Context
import android.content.Intent
import androidx.annotation.RequiresApi
import androidx.annotation.StringRes
import be.mygod.vpnhotspot.shizuku.OwnedState
import be.mygod.vpnhotspot.shizuku.ShizukuLifecycle
import be.mygod.vpnhotspot.shizuku.ShizukuTestNetwork
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import timber.log.Timber

/**
 * Holds one rootless Shizuku session for as long as it runs, and owns nothing else.
 *
 * [ShizukuTestNetwork] is the session owner - the TUN, the exact request, the agent, the upstream
 * observation and the child - and this component owns the *lifespan* that drives it: one cancellable job on
 * this service's own scope, exactly as `RepeaterService` owns its own. What that buys is a process lifetime.
 * Without a foreground component Android may reclaim the process at any moment, which withdraws the agent,
 * hands the hotspot silently back to an ordinary upstream and strands the global preference. The design
 * accepts that as an abnormal outcome with explicit manual recovery; it must not be the ordinary one, so the
 * session runs behind the same foreground notification every other long-lived thing in this app uses, from
 * before authorization until after the last confirmed teardown step.
 *
 * Neither command blocks. A start installs a lifespan and returns, a stop cancels one and returns, and the
 * teardown that follows runs in the lifespan's own finalizer behind this service's importance. `onDestroy`
 * therefore leaves the scope alive rather than cutting it off, and lets a
 * [kotlinx.coroutines.NonCancellable] finalizer end it when it is done - the component is gone by then,
 * which is the abnormal path this design accepts, exactly as `RepeaterService` accepts it.
 *
 * Which finalizer that is, is the whole of what `onDestroy` decides, and it is not the outgoing lifespan's.
 * [ShizukuLifecycle.destroy] installs a cleanup-only successor *first* and only then withdraws and cancels
 * the lifespan it replaced, so the successor - never the lifespan whose retirement may itself have failed -
 * is what makes the last cleanup attempt this process gets and what ends this scope. A duplicate press or an
 * idle stop gets that same lifespan from [ShizukuLifecycle.housekeep], which cancels nothing because there
 * is nothing to cancel.
 *
 * There is exactly one current liveness authority throughout. An outgoing predecessor may still finish
 * behind its installed successor, but it can no longer wind this component down; every ledger access stays
 * inside that one ordered lifespan chain rather than beside it in another owner. This closes three holes: a
 * service left idle over debt a teardown could not fence would have no owner for it, one destroyed over debt
 * its *own* retirement could not fence would settle unfenced and abandon it, and one recreated behind a
 * finalizer that is still running would read the ledger straight through it.
 *
 * This mode never starts or stops tethering, and it neither starts, stops nor consults root mode: the two run
 * independently, and root's own routing takes precedence over whatever upstream Android selected whenever
 * both are up. Unlike its siblings this service owns no downstream interface, no neighbour monitoring and no
 * client counts, and it contributes nothing to what the shared notification says: it registers with
 * [ServiceNotification] exactly as an interfaceless service already does, which keeps the notification alive
 * without changing a word of it. What this mode has to say about its own state belongs in its own row, which
 * is the surface it owns.
 *
 * It deliberately does not persist across reboot: a session needs Shizuku authorized and an interactive
 * failure path, and nothing in the design asks for one to be replayed.
 */
@RequiresApi(33)
class ShizukuTetheringService : Service(), CoroutineScope {
    /**
     * What the user is shown, and nothing else: whether this mode is meant to be on, and what a *committed*
     * session of that same lifespan is doing.
     *
     * There is no transition here and no busy flag. A start is accepted immediately and a stop takes effect
     * immediately; whatever either still has to finish is this mode's own business, not something to make
     * the user watch with a control they cannot press.
     */
    data class Status(@StringRes val label: Int?, val on: Boolean)

    companion object {
        private const val EXTRA_STOP = "shizuku.stop"

        val status = combine(ShizukuTestNetwork.intent, ShizukuTestNetwork.state) { owner, committed ->
            // A label belongs to a committed session of the lifespan that is currently the accepted one, by
            // identity. Off never carries one, so a row that reads off can never also read Active, and a
            // predecessor's late write can never label a successor.
            Status(OwnedState.label(owner, committed), owner != null)
        }.distinctUntilChanged()

        /**
         * The one way in. Both directions go through this component rather than straight to the lifespan, so
         * the session always has foreground importance behind it - including during a teardown, which can
         * take minutes.
         */
        fun start(context: Context) = context.startForegroundService(
            Intent(context, ShizukuTetheringService::class.java))
        fun stop(context: Context) = context.startForegroundService(
            Intent(context, ShizukuTetheringService::class.java).putExtra(EXTRA_STOP, true))
    }

    override val coroutineContext = Dispatchers.Main.immediate + Job()

    /**
     * True once Android has removed this component. The lifespan still finishes; what changes is how it
     * winds down, because there is no longer a service to stop.
     */
    private var destroyed = false

    /**
     * The most recent delivered start id, so a wind-down cannot destroy a command that arrived after the
     * lifespan it belongs to had already ended: `stopSelf(startId)` is a no-op once a newer one is in.
     */
    private var startId = 0

    private val lifecycle = ShizukuLifecycle(this, object : ShizukuLifecycle.Session {
        override suspend fun settle() = ShizukuTestNetwork.settle()
        override suspend fun prepare() = ShizukuTestNetwork.prepare()
        override suspend fun awaitEnd() = ShizukuTestNetwork.awaitEnd()
        override suspend fun retire(owner: Job) = ShizukuTestNetwork.stop(owner)
        override fun publish(owner: Job) = ShizukuTestNetwork.publishIntent(owner)
        override fun withdraw(owner: Job) = ShizukuTestNetwork.withdrawIntent(owner)

        override suspend fun fenced() = ShizukuTestNetwork.localResourcesFenced()

        override fun report(e: Exception) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
        }

        override fun settled(fenced: Boolean) {
            // Nothing local could be fenced, so a child or an agent may still be relaying: the process keeps
            // owning it. Importance stays and the row stays off - the debt is internal, not something the
            // user is shown as still on - and the next command retries it, a start in front of its own
            // preparation and an idle stop as the whole of its housekeeping lifespan. Deliberately not a
            // retry of its own: without a command there is nothing new to try, and reissuing the same
            // attempt on a timer is the loop this design does not have.
            //
            // Only a component Android has already removed gives up here. It has made its attempt by then -
            // `onDestroy` installs a housekeeping lifespan for exactly that - and past this point there is
            // no importance left to hold the process with, so what owns the residue is process death,
            // explicitly and exactly as it does for a force stop.
            if (!fenced && !destroyed) return
            if (destroyed) cancel() else stopSelf(startId)
        }
    })

    override fun onBind(intent: Intent?) = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // First, and before anything that can fail: startForegroundService promises this within its window,
        // and the whole point of routing commands through here is that the window covers them.
        ServiceNotification.startForeground(this)
        this.startId = startId
        // Neither blocks. A press while intent is already on is that same intent and installs nothing.
        if (intent?.getBooleanExtra(EXTRA_STOP, false) == true) lifecycle.stop()
        else if (ShizukuTestNetwork.intent.value == null) lifecycle.start()
        // A command that installed and cancelled nothing - a duplicate or idle stop - leaves no lifespan to
        // wind this service down, and it would otherwise stay foreground forever waiting for one. Idle is
        // not the same as owing nothing, though, so this cannot simply stop either: it gets a lifespan of
        // its own, which joins whatever is still finalizing anywhere in this process, makes one attempt at
        // what that left, and settles this component from the ledger's answer through the same [settled]
        // below. One owner, never two, and never a question asked beside the owner: a query launched here
        // would read the ledger straight through a destroyed instance's finalizer, see the resources it is
        // in the middle of releasing, and strand this service on what it saw.
        if (lifecycle.idle) lifecycle.housekeep()
        return START_NOT_STICKY
    }

    /**
     * A teardown this service did not ask for - `stopService` from elsewhere, or the system removing the
     * component - still has to withdraw the session, because the agent, the request, the preference and the
     * child all outlive this component otherwise.
     *
     * One command, and not a branch on whether anything happens to be in flight. Destruction is the *last*
     * command this owner gets, so it always leaves exactly one cleanup-only lifespan behind: an installed
     * lifespan is withdrawn and cancelled rather than cut off, that successor is installed over it first and
     * joins it, and the scope is left alive until the successor has made its one attempt at whatever the
     * cancelled one could not fence and ended the scope itself through
     * [ShizukuLifecycle.Session.settled].
     *
     * Both halves matter, and neither covers the other. Without the successor a retirement that fails *under*
     * a destruction settles unfenced, ends the scope and abandons the debt with nothing left in this process
     * to attempt it. Without the cancellation the session would simply go on running behind a component that
     * no longer exists. And idle is not the same as owing nothing either, so a destruction that finds nothing
     * in flight still installs the successor rather than ending the scope at once: what a previous teardown
     * could not fence is still there, and Android removing this component does not remove the process, which
     * an activity or another foreground service may well be holding up. Either way this delays the
     * *completion* of this instance's own scope past `onDestroy`; it does not hand the withdrawal to any
     * other scope, and no scope outlives the process.
     *
     * Foreground importance cannot be kept past this point - the framework releases it with the component -
     * so this is the abnormal path the design accepts, exactly as `RepeaterService` accepts it, and the
     * ordinary stop is the one that holds importance throughout. That is also why the successor above makes
     * one attempt and not a policy: what it cannot fence, nothing here can, and process death owns the
     * residue.
     * Nothing about resource debt is remembered here: [ShizukuTestNetwork]'s ledger is the process's, not
     * this component's, so a service recreated in the same process settles what is left the way any command
     * does, through [ShizukuTestNetwork.settle]. Nothing about *ordering* is kept here either, and for the
     * same reason: the cleanup-only lifespan left in flight above is the predecessor a recreated instance's
     * first command joins, which [ShizukuLifecycle] can promise because that barrier is the process's rather
     * than any one instance's. Process death is the final local fence, and it takes the resources with it.
     */
    override fun onDestroy() {
        destroyed = true
        ServiceNotification.stopForeground(this)
        lifecycle.destroy()
        super.onDestroy()
    }
}
