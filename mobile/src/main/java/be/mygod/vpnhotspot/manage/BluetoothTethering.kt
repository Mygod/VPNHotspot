package be.mygod.vpnhotspot.manage

import android.annotation.TargetApi
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothProfile
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.widget.Toast
import androidx.annotation.RequiresApi
import androidx.core.os.BuildCompat
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.io.IOException
import java.lang.reflect.InvocationTargetException

class BluetoothTethering(context: Context, val stateListener: (Int) -> Unit) :
        BluetoothProfile.ServiceListener, AutoCloseable {
    companion object : BroadcastReceiver() {
        /**
         * PAN Profile
         * From BluetoothProfile.java.
         */
        private const val PAN = 5
        private val isTetheringOn by lazy {
            Class.forName("android.bluetooth.BluetoothPan").getDeclaredMethod("isTetheringOn")
        }

        private fun registerBluetoothStateListener(receiver: BroadcastReceiver) =
                app.registerReceiver(receiver, IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED))
        private val Intent.bluetoothState get() = getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)

        private var pendingCallback: TetheringManager.StartTetheringCallback? = null

        /**
         * https://android.googlesource.com/platform/packages/apps/Settings/+/b1af85d/src/com/android/settings/TetherSettings.java#215
         */
        @TargetApi(24)
        override fun onReceive(context: Context?, intent: Intent?) {
            when (intent?.bluetoothState) {
                BluetoothAdapter.STATE_ON -> try {
                    TetheringManager.startTethering(TetheringManager.TETHERING_BLUETOOTH, true, pendingCallback!!)
                } catch (e: IOException) {
                    Timber.w(e)
                    Toast.makeText(context, e.readableMessage, Toast.LENGTH_LONG).show()
                    pendingCallback!!.onException()
                } catch (e: InvocationTargetException) {
                    if (e.targetException !is SecurityException) Timber.w(e)
                    var cause: Throwable? = e
                    while (cause != null) {
                        cause = cause.cause
                        if (cause != null && cause !is InvocationTargetException) {
                            Toast.makeText(context, cause.readableMessage, Toast.LENGTH_LONG).show()
                            pendingCallback!!.onException()
                            break
                        }
                    }
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
            if (adapter?.state == BluetoothAdapter.STATE_OFF) {
                registerBluetoothStateListener(this)
                pendingCallback = callback
                adapter.enable()
            } else TetheringManager.startTethering(TetheringManager.TETHERING_BLUETOOTH, true, callback)
        }
    }

    private var pan: BluetoothProfile? = null
    /**
     * Based on: https://android.googlesource.com/platform/packages/apps/Settings/+/78d5efd/src/com/android/settings/TetherSettings.java
     */
    val active: Boolean? get() {
        val pan = pan ?: return null
        return BluetoothAdapter.getDefaultAdapter()?.state == BluetoothAdapter.STATE_ON && try {
            isTetheringOn.invoke(pan) as Boolean
        } catch (e: InvocationTargetException) {
            if (e.cause is SecurityException && BuildCompat.isAtLeastR()) Timber.d(e) else Timber.w(e)
            return null
        }
    }

    private val receiver = broadcastReceiver { _, intent -> stateListener(intent.bluetoothState) }

    init {
        try {
            BluetoothAdapter.getDefaultAdapter()?.getProfileProxy(context, this, PAN)
        } catch (e: SecurityException) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
        }
        registerBluetoothStateListener(receiver)
    }

    override fun onServiceDisconnected(profile: Int) {
        pan = null
    }
    override fun onServiceConnected(profile: Int, proxy: BluetoothProfile) {
        pan = proxy
    }
    override fun close() {
        app.unregisterReceiver(receiver)
        BluetoothAdapter.getDefaultAdapter()?.closeProfileProxy(PAN, pan)
        pan = null
    }
}
