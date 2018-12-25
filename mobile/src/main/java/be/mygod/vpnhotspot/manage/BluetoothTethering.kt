package be.mygod.vpnhotspot.manage

import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothProfile
import android.content.Context
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber

class BluetoothTethering(context: Context) : BluetoothProfile.ServiceListener, AutoCloseable {
    companion object {
        /**
         * PAN Profile
         * From BluetoothProfile.java.
         */
        private const val PAN = 5
        private val isTetheringOn by lazy @SuppressLint("PrivateApi") {
            Class.forName("android.bluetooth.BluetoothPan").getDeclaredMethod("isTetheringOn")
        }
    }

    private var pan: BluetoothProfile? = null
    /**
     * Based on: https://android.googlesource.com/platform/packages/apps/Settings/+/78d5efd/src/com/android/settings/TetherSettings.java
     */
    val active: Boolean? get() {
        val pan = pan ?: return null
        return BluetoothAdapter.getDefaultAdapter()?.state == BluetoothAdapter.STATE_ON &&
                isTetheringOn.invoke(pan) as Boolean
    }

    init {
        try {
            BluetoothAdapter.getDefaultAdapter()?.getProfileProxy(context, this, PAN)
        } catch (e: SecurityException) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
        }
    }

    override fun onServiceDisconnected(profile: Int) {
        pan = null
    }
    override fun onServiceConnected(profile: Int, proxy: BluetoothProfile) {
        pan = proxy
    }
    override fun close() {
        BluetoothAdapter.getDefaultAdapter()?.closeProfileProxy(PAN, pan)
        pan = null
    }
}
