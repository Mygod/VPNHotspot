package be.mygod.vpnhotspot

import android.app.Service
import android.content.Context
import android.content.Intent
import androidx.annotation.RequiresApi
import androidx.annotation.StringRes
import be.mygod.vpnhotspot.shizuku.ShizukuLifecycle
import be.mygod.vpnhotspot.shizuku.ShizukuTestNetwork
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.DelicateCoroutinesApi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import timber.log.Timber

/**
 * Holds one rootless Shizuku session for as long as it runs, and owns nothing else.
 *
 * [ShizukuTestNetwork] is the session owner - the TUN, the exact request, the agent, the upstream observation
 * and the child - and [ShizukuTestNetwork.lifecycle] is its command lane. What this component adds is a
 * process lifetime. Without a foreground component Android may reclaim the process at
 * any moment, which withdraws the agent, hands the hotspot silently back to an ordinary upstream and strands
 * the global preference. The design accepts that as an abnormal outcome with explicit manual recovery; it must
 * not be the ordinary one, so the session runs behind the same foreground notification every other long-lived
 * thing in this app uses, from before authorization until after the last confirmed teardown step.
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
     * What the user is shown and what the one command path is allowed to do.
     *
     * Two Shizuku-local owners contribute to it and neither alone is the answer. [ShizukuTestNetwork.lifecycle]
     * knows a command is running - which is the whole authorization and startup window, where the session
     * state is still null and rendering that as off is what let the old UI accept a second start - while
     * [ShizukuTestNetwork.state] knows what a committed session is actually doing.
     */
    data class Status(@StringRes val label: Int?, val on: Boolean, val busy: Boolean)

    companion object {
        private const val EXTRA_STOP = "shizuku.stop"

        val status = combine(ShizukuTestNetwork.lifecycle.state, ShizukuTestNetwork.state) { mode, session ->
            Status(
                label = session?.label ?: when (mode) {
                    // Authorization is starting, and saying so is the whole point of the named state: this
                    // window can sit on the user's own Shizuku permission dialog, and an earlier version
                    // rendered it as off, which let the row accept a second start.
                    ShizukuLifecycle.State.PREPARING, ShizukuLifecycle.State.PUBLISHING ->
                        R.string.shizuku_state_starting
                    ShizukuLifecycle.State.RETIRING -> R.string.shizuku_state_stopping
                    else -> null
                },
                // "On" is "this mode has a session or is creating one", which is exactly what its own command
                // lane reports. A session that ended on its own has already finished withdrawing.
                on = mode.on,
                // No new command is accepted while one is running. It would not be lost - the lane shares,
                // queues or supersedes it deterministically - but offering it invites the user to fight
                // their own last press, and a start-during-stop is not a thing this mode should decide
                // silently. Busy therefore covers the whole of a supersession too: an explicit stop that
                // caught a start in its interactive half reads as stopping until that start has unwound.
                //
                // Deliberately *not* [ShizukuTestNetwork.State.STOPPING]: a withdrawal that could not fence
                // its child or prove its network destroyed leaves that state behind with the lane back in
                // [ShizukuLifecycle.State.ON], and disabling the only control then would disable it forever.
                // The control has to keep working, because pressing it again is the retry.
                busy = mode.busy,
            )
        }.distinctUntilChanged()

        /**
         * The one ordered command path. Both directions go through this component rather than straight to
         * [ShizukuTestNetwork.lifecycle], so the session always has foreground importance behind it -
         * including during a stop, whose confirmed teardown can take minutes.
         */
        fun start(context: Context) = context.startForegroundService(
            Intent(context, ShizukuTetheringService::class.java))
        fun stop(context: Context) = context.startForegroundService(
            Intent(context, ShizukuTetheringService::class.java).putExtra(EXTRA_STOP, true))
    }

    override val coroutineContext = Dispatchers.Main.immediate + Job()

    override fun onBind(intent: Intent?) = null

    override fun onCreate() {
        super.onCreate()
        launch {
            // Only onStartCommand can turn the mode on, so an initial "off" means "not started yet" rather
            // than "finished" and must not stop the service. A command that finds the mode already off never
            // moves this at all, which is why onStartCommand checks for itself rather than leaving the
            // decision here: waiting on an event that has already happened waits forever.
            status.first { it.on }
            // Nothing is left to hold the process for once the mode is off and no command is running, whether
            // the user stopped it or the session ended on its own.
            status.first { !it.on && !it.busy }
            stopSelf()
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // First, and before anything that can fail: startForegroundService promises this within its window,
        // and the whole point of routing commands through here is that the window covers them.
        ServiceNotification.startForeground(this)
        launch {
            try {
                // Deterministic rather than racy in either direction, and the direction matters: a stop
                // issued while a start is still in its interactive half *supersedes* it - nothing has been
                // created, so nothing is published and then immediately torn down - while one issued during
                // publication queues on the command lane and withdraws the session the moment it lands. A
                // duplicate start shares the start already in flight instead of beginning a second one.
                if (intent?.getBooleanExtra(EXTRA_STOP, false) == true) ShizukuTestNetwork.lifecycle.stop()
                else ShizukuTestNetwork.lifecycle.start()
            } catch (e: Exception) {
                // A deadline is an operational failure however it is shaped, and `withTimeout` shapes it as
                // a [TimeoutCancellationException]. Catching every [CancellationException] ahead of the
                // report below therefore sent the real failures silently past it - an agent that never
                // published, a child that never completed its handshake, a Shizuku authorization that never
                // came back, a privileged release that could not be reissued - and past the check after it,
                // leaving the service foreground with nothing running. Every *other* cancellation here is
                // genuine and stays quiet: this service's own job going away, and the
                // [ShizukuLifecycle.SupersededException] a start receives when an explicit stop caught it
                // before it created anything, which is the stop working rather than a failure to report.
                if (e is CancellationException && e !is TimeoutCancellationException) throw e
                // A startup failure is terminal and has already rolled back everything it created; a stop
                // failure has kept everything it could not release. Either has to be visible rather than
                // only logged, and the row's own control stays enabled so the user can retry.
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }
            // The command has finished, so this is the moment the answer is knowable: the service exists only
            // to hold a running session, and there is nothing left to hold once the lane reports neither a
            // session nor a command in flight. Checked here rather than left to the collector because a
            // command that changed nothing - a stop arriving when the mode was already off, which makes
            // [ShizukuLifecycle.stop] a no-op - publishes no new state for a collector to observe, and the
            // service would stay foreground forever waiting for one. Deliberately *not* unconditional on the
            // failure path either: a stop that could not withdraw its session leaves it running, and stopping
            // the service then would take away the only thing keeping it alive.
            ShizukuTestNetwork.lifecycle.state.value.let { if (!it.on && !it.busy) stopSelf() }
        }
        return START_NOT_STICKY
    }

    /**
     * A teardown this service did not ask for - `stopService` from elsewhere, or the system removing the
     * component - still has to withdraw the session, because the agent, the request, the preference and the
     * child all outlive this component otherwise.
     *
     * The withdrawal therefore runs in a scope that outlives this service's own [Job], which is being
     * cancelled: launching it here would abandon the preference clear, the destruction barrier and the
     * request release at their first suspension. Foreground importance cannot be kept past `onDestroy` - the
     * framework releases it with the component - so this is the abnormal path the design accepts, and the
     * ordinary stop above is the one that holds importance throughout.
     */
    @OptIn(DelicateCoroutinesApi::class)
    override fun onDestroy() {
        ServiceNotification.stopForeground(this)
        // [ShizukuLifecycle.State.on] rather than a committed session: a component destroyed during the
        // authorization window has to supersede that start too, because nothing else would - the flight is
        // owned by the lane and would otherwise commit a session with no foreground component behind it.
        // Shizuku-local, like every other path here: root mode is not asked for anything.
        if (ShizukuTestNetwork.lifecycle.state.value.on) GlobalScope.launch {
            try {
                ShizukuTestNetwork.lifecycle.stop()
            } catch (e: Exception) {
                // Same rule as the command path above, and for the same reason: a withdrawal that ran out of
                // time is the one thing here worth reporting, and it arrives as a cancellation.
                if (e is CancellationException && e !is TimeoutCancellationException) throw e
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }
        }
        cancel()
        super.onDestroy()
    }
}
