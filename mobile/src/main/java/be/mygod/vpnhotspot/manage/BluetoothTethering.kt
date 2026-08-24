package be.mygod.vpnhotspot.manage

import android.annotation.SuppressLint
import android.app.Activity
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothPan
import android.bluetooth.BluetoothProfile
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.Build
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.ensureReceiverUnregistered
import be.mygod.vpnhotspot.util.readableMessage
import kotlinx.coroutines.suspendCancellableCoroutine
import timber.log.Timber
import kotlin.coroutines.resume

class BluetoothTethering(private val adapter: BluetoothAdapter, val stateListener: () -> Unit) :
        BluetoothProfile.ServiceListener, AutoCloseable {
    companion object {
        /**
         * PAN Profile
         */
        private const val PAN = 5

        private fun registerBluetoothStateListener(receiver: BroadcastReceiver) =
                app.registerReceiver(receiver, IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED))
    }

    private var proxyCreated = false
    private var connected = false
    private var closed = false
    private var awaitingBluetooth = false
    private var pan: BluetoothPan? = null
    var activeFailureCause: Throwable? = null
    /**
     * Based on: https://android.googlesource.com/platform/packages/apps/Settings/+/78d5efd/src/com/android/settings/TetherSettings.java
     */
    val active: Boolean? get() {
        val pan = pan ?: return null
        if (!connected) return null
        activeFailureCause = null
        return adapter.state == BluetoothAdapter.STATE_ON && try {
            pan.isTetheringOn
        } catch (e: SecurityException) {
            activeFailureCause = e
            if (Build.VERSION.SDK_INT >= 30) Timber.d(e.readableMessage) else Timber.w(e)
            return null
        }
    }

    private val receiver = broadcastReceiver { _, _ -> stateListener() }

    fun ensureInit() {
        if (closed) return
        activeFailureCause = null
        if (!proxyCreated) try {
            if (adapter.getProfileProxy(app, this, PAN)) proxyCreated = true
            else activeFailureCause = Exception("getProfileProxy failed")
        } catch (e: SecurityException) {
            if (Build.VERSION.SDK_INT >= 31) Timber.d(e.readableMessage) else Timber.w(e)
            activeFailureCause = e
        }
    }
    init {
        ensureInit()
        registerBluetoothStateListener(receiver)
    }

    override fun onServiceDisconnected(profile: Int) {
        connected = false
    }
    override fun onServiceConnected(profile: Int, proxy: BluetoothProfile) {
        if (closed) {
            adapter.closeProfileProxy(PAN, proxy)
            return
        }
        pan = proxy as BluetoothPan
        connected = true
        stateListener()
    }

    /**
     * https://android.googlesource.com/platform/packages/apps/Settings/+/b1af85d/src/com/android/settings/TetherSettings.java#384
     */
    @SuppressLint("MissingPermission")
    suspend fun start(context: Context) {
        // Turning Bluetooth on is asynchronous, so wait for STATE_ON before tethering. A STATE_OFF/ERROR
        // outcome (e.g. the user declined the enable prompt) aborts silently, like the old receiver did.
        if (adapter.state == BluetoothAdapter.STATE_OFF) {
            if (awaitingBluetooth) return
            awaitingBluetooth = true
            val on = try {
                awaitBluetoothOn(context)
            } finally {
                awaitingBluetooth = false
            }
            if (!on) return
        }
        TetheringManagerCompat.startTethering(TetheringManagerCompat.TETHERING_BLUETOOTH, true)
    }
    suspend fun stop() = TetheringManagerCompat.stopTethering(TetheringManagerCompat.TETHERING_BLUETOOTH)

    @SuppressLint("MissingPermission")
    private suspend fun awaitBluetoothOn(context: Context): Boolean = suspendCancellableCoroutine { cont ->
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context?, intent: Intent?) {
                when (intent?.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)) {
                    BluetoothAdapter.STATE_ON -> {
                        app.ensureReceiverUnregistered(this)
                        cont.resume(true)
                    }
                    BluetoothAdapter.STATE_OFF, BluetoothAdapter.ERROR -> {
                        app.ensureReceiverUnregistered(this)
                        cont.resume(false)
                    }
                    // else: transition state, keep waiting
                }
            }
        }
        registerBluetoothStateListener(receiver)
        cont.invokeOnCancellation { app.ensureReceiverUnregistered(receiver) }
        try {
            @Suppress("DEPRECATION")
            if (!adapter.enable()) context.startActivity(Intent(BluetoothAdapter.ACTION_REQUEST_ENABLE).apply {
                if (context !is Activity) addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            })
        } catch (e: Throwable) {
            app.ensureReceiverUnregistered(receiver)
            throw e
        }
    }

    override fun close() {
        closed = true
        app.unregisterReceiver(receiver)
        pan?.let { adapter.closeProfileProxy(PAN, it) }
        pan = null
        connected = false
    }
}
