package be.mygod.vpnhotspot

import android.app.Service
import android.content.Context
import android.content.Intent
import androidx.annotation.RequiresApi
import androidx.annotation.StringRes
import be.mygod.vpnhotspot.shizuku.OwnedState
import be.mygod.vpnhotspot.shizuku.ShizukuLifecycle
import be.mygod.vpnhotspot.shizuku.ShizukuTestNetwork
import be.mygod.vpnhotspot.shizuku.UnsupportedDeviceException
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import timber.log.Timber

/** Foreground-service owner for one [ShizukuLifecycle] lifespan and any inherited cleanup debt. */
@RequiresApi(30)
class ShizukuTetheringService : Service(), CoroutineScope {
    data class Status(@StringRes val label: Int?, val on: Boolean)

    companion object {
        private const val EXTRA_STOP = "shizuku.stop"

        val status = combine(ShizukuTestNetwork.intent, ShizukuTestNetwork.state) { owner, committed ->
            Status(OwnedState.label(owner, committed), owner != null)
        }.distinctUntilChanged()

        fun start(context: Context) = context.startForegroundService(
            Intent(context, ShizukuTetheringService::class.java))
        fun stop(context: Context) = context.startForegroundService(
            Intent(context, ShizukuTetheringService::class.java).putExtra(EXTRA_STOP, true))
    }

    override val coroutineContext = Dispatchers.Main.immediate + Job()

    private var destroyed = false

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
            if (e is UnsupportedDeviceException && e.expected) Timber.d(e) else Timber.w(e)
            SmartSnackbar.make(e).show()
        }

        override fun settled(fenced: Boolean) {
            if (!fenced && !destroyed) return
            if (destroyed) cancel() else stopSelf(startId)
        }
    })

    override fun onBind(intent: Intent?) = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        ServiceNotification.startForeground(this)
        this.startId = startId
        if (intent?.getBooleanExtra(EXTRA_STOP, false) == true) lifecycle.stop()
        else if (ShizukuTestNetwork.intent.value == null) lifecycle.start()
        if (lifecycle.idle) lifecycle.housekeep()
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        destroyed = true
        ServiceNotification.stopForeground(this)
        lifecycle.destroy()
        super.onDestroy()
    }
}
