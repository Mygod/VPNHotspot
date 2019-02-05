package be.mygod.vpnhotspot.net.wifi

import android.annotation.SuppressLint
import android.content.SharedPreferences
import android.net.wifi.WifiManager
import android.os.PowerManager
import androidx.core.content.getSystemService
import be.mygod.vpnhotspot.App.Companion.app

/**
 * This mechanism is used to maximize profit. Source: https://stackoverflow.com/a/29657230/2245107
 */
class WifiDoubleLock(lockType: Int) : AutoCloseable {
    companion object : SharedPreferences.OnSharedPreferenceChangeListener {
        private const val KEY = "service.wifiLock"
        private val lockType get() =
            WifiDoubleLock.Mode.valueOf(app.pref.getString(KEY, WifiDoubleLock.Mode.Full.toString()) ?: "").lockType
        private val service by lazy { app.getSystemService<PowerManager>()!! }

        private var holders = mutableSetOf<Any>()
        private var lock: WifiDoubleLock? = null

        fun acquire(holder: Any) = synchronized(this) {
            if (holders.isEmpty()) {
                app.pref.registerOnSharedPreferenceChangeListener(this)
                val lockType = lockType
                if (lockType != null) lock = WifiDoubleLock(lockType)
            }
            check(holders.add(holder))
        }
        fun release(holder: Any) = synchronized(this) {
            check(holders.remove(holder))
            if (holders.isEmpty()) {
                lock?.close()
                lock = null
                app.pref.unregisterOnSharedPreferenceChangeListener(this)
            }
        }

        override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
            if (key == KEY) synchronized(this) {
                lock?.close()
                val lockType = lockType
                lock = if (lockType == null) null else WifiDoubleLock(lockType)
            }
        }
    }

    enum class Mode(val lockType: Int? = null) {
        None, Full(WifiManager.WIFI_MODE_FULL), HighPerf(WifiManager.WIFI_MODE_FULL_HIGH_PERF)
    }

    private val wifi = app.wifi.createWifiLock(lockType, "vpnhotspot:wifi").apply { acquire() }
    @SuppressLint("WakelockTimeout")
    private val power = service.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "vpnhotspot:power").apply { acquire() }

    override fun close() {
        wifi.release()
        power.release()
    }
}
