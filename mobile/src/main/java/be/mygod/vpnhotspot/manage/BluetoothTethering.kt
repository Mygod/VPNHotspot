package be.mygod.vpnhotspot.manage

import android.annotation.TargetApi
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothProfile
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.Build
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.lang.reflect.InvocationTargetException

class BluetoothTethering(context: Context, val stateListener: () -> Unit) :
        BluetoothProfile.ServiceListener, AutoCloseable {
    companion object : BroadcastReceiver() {
        /**
         * PAN Profile
         */
        private const val PAN = 5
        private val clazz by lazy { Class.forName("android.bluetooth.BluetoothPan") }
        private val constructor by lazy {
            clazz.getDeclaredConstructor(Context::class.java, BluetoothProfile.ServiceListener::class.java).apply {
                isAccessible = true
            }
        }
        private val isTetheringOn by lazy { clazz.getDeclaredMethod("isTetheringOn") }

        fun pan(context: Context, serviceListener: BluetoothProfile.ServiceListener) =
                constructor.newInstance(context, serviceListener) as BluetoothProfile
        val BluetoothProfile.isTetheringOn get() = isTetheringOn(this) as Boolean
        fun BluetoothProfile.closePan() = BluetoothAdapter.getDefaultAdapter()!!.closeProfileProxy(PAN, this)

        private fun registerBluetoothStateListener(receiver: BroadcastReceiver) =
                app.registerReceiver(receiver, IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED))

        private var pendingCallback: TetheringManager.StartTetheringCallback? = null

        /**
         * https://android.googlesource.com/platform/packages/apps/Settings/+/b1af85d/src/com/android/settings/TetherSettings.java#215
         */
        @TargetApi(24)
        override fun onReceive(context: Context?, intent: Intent?) {
            when (intent?.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)) {
                BluetoothAdapter.STATE_ON -> {
                    TetheringManager.startTethering(TetheringManager.TETHERING_BLUETOOTH, true, pendingCallback!!)
                }
                BluetoothAdapter.STATE_OFF, BluetoothAdapter.ERROR -> { }
                else -> return  // ignore transition states
            }
            pendingCallback = null
            app.unregisterReceiver(this)
        }

        /**
         * https://android.googlesource.com/platform/packages/apps/Settings/+/b1af85d/src/com/android/settings/TetherSettings.java#384
         */
        @RequiresApi(24)
        fun start(callback: TetheringManager.StartTetheringCallback) {
            if (pendingCallback != null) return
            val adapter = BluetoothAdapter.getDefaultAdapter()
            try {
                if (adapter?.state == BluetoothAdapter.STATE_OFF) {
                    registerBluetoothStateListener(this)
                    pendingCallback = callback
                    adapter.enable()
                } else TetheringManager.startTethering(TetheringManager.TETHERING_BLUETOOTH, true, callback)
            } catch (e: SecurityException) {
                SmartSnackbar.make(e.readableMessage).shortToast().show()
            }
        }
    }

    private var connected = false
    private var pan: BluetoothProfile? = null
    var activeFailureCause: Throwable? = null
    /**
     * Based on: https://android.googlesource.com/platform/packages/apps/Settings/+/78d5efd/src/com/android/settings/TetherSettings.java
     */
    val active: Boolean? get() {
        val pan = pan ?: return null
        if (!connected) return null
        activeFailureCause = null
        return BluetoothAdapter.getDefaultAdapter()?.state == BluetoothAdapter.STATE_ON && try {
            pan.isTetheringOn
        } catch (e: InvocationTargetException) {
            activeFailureCause = e.cause ?: e
            if (e.cause is SecurityException && Build.VERSION.SDK_INT >= 30) Timber.d(e.readableMessage)
            else Timber.w(e)
            return null
        }
    }

    private val receiver = broadcastReceiver { _, _ -> stateListener() }

    fun ensureInit(context: Context) {
        if (pan == null && BluetoothAdapter.getDefaultAdapter() != null) try {
            pan = pan(context, this)
        } catch (e: InvocationTargetException) {
            Timber.w(e)
            activeFailureCause = e
        }
    }
    init {
        ensureInit(context)
        registerBluetoothStateListener(receiver)
    }

    override fun onServiceDisconnected(profile: Int) {
        connected = false
    }
    override fun onServiceConnected(profile: Int, proxy: BluetoothProfile) {
        connected = true
        stateListener()
    }
    override fun close() {
        app.unregisterReceiver(receiver)
        pan?.closePan()
    }
}
