package be.mygod.vpnhotspot.util

import android.annotation.SuppressLint
import android.app.PendingIntent
import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.os.Build
import android.os.DeadObjectException
import android.os.IBinder
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import be.mygod.vpnhotspot.BootReceiver
import java.lang.ref.WeakReference

abstract class KillableTileService : TileService(), ServiceConnection {
    protected var tapPending = false

    /**
     * Compat helper for setSubtitle.
     */
    protected fun Tile.subtitle(value: CharSequence?) {
        if (Build.VERSION.SDK_INT >= 29) subtitle = value
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        if (tapPending) {
            tapPending = false
            onClick()
        }
    }

    override fun onBind(intent: Intent?) = try {
        super.onBind(intent)
    } catch (e: RuntimeException) {
        if (e.cause !is DeadObjectException) throw e
        null
    }

    protected fun runActivity(intent: Intent) = unlockAndRun {
        if (Build.VERSION.SDK_INT < 34) @Suppress("DEPRECATION") @SuppressLint("StartActivityAndCollapseDeprecated") {
            startActivityAndCollapse(intent)
        } else startActivityAndCollapse(PendingIntent.getActivity(this, 0, intent, PendingIntent.FLAG_IMMUTABLE))
    }
    fun dismiss() = runActivity(Intent(this, SelfDismissActivity::class.java).setFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    @Suppress("LeakingThis")
    protected val dismissHandle = WeakReference(this)
    override fun onDestroy() {
        dismissHandle.clear()
        super.onDestroy()
    }
}
typealias TileServiceDismissHandle = WeakReference<KillableTileService>
