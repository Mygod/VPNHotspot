package be.mygod.vpnhotspot.net.wifi

import android.annotation.SuppressLint
import android.content.SharedPreferences
import android.net.wifi.WifiManager
import android.os.Build
import android.os.PowerManager
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.annotation.RequiresApi
import androidx.core.content.edit
import androidx.core.content.getSystemService
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services

/**
 * This mechanism is used to maximize profit. Source: https://stackoverflow.com/a/29657230/2245107
 */
class WifiDoubleLock(lockType: Int) : AutoCloseable {
    companion object : SharedPreferences.OnSharedPreferenceChangeListener {
        private const val KEY = "service.wifiLock"
        var mode: Mode
            @Suppress("DEPRECATION")
            get() = Mode.valueOf(app.pref.getString(KEY, Mode.Full.toString()) ?: "").let {
                if (it == Mode.Full && Build.VERSION.SDK_INT >= 29) Mode.None else it
            }
            set(value) = app.pref.edit { putString(KEY, value.toString()) }
        private val service by lazy { app.getSystemService<PowerManager>()!! }

        private var holders = mutableSetOf<Any>()
        private var lock: WifiDoubleLock? = null

        fun acquire(holder: Any) = synchronized(this) {
            if (holders.isEmpty()) {
                app.pref.registerOnSharedPreferenceChangeListener(this)
                val lockType = mode.lockType
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
                val lockType = mode.lockType
                lock = if (lockType == null) null else WifiDoubleLock(lockType)
            }
        }
    }

    enum class Mode(val lockType: Int? = null, val keepScreenOn: Boolean = false) {
        None,
        @Suppress("DEPRECATION")
        @Deprecated("This constant was deprecated in API level Q.\n" +
                "This API is non-functional and will have no impact.")
        Full(WifiManager.WIFI_MODE_FULL),
        @Suppress("DEPRECATION")
        HighPerf(WifiManager.WIFI_MODE_FULL_HIGH_PERF),
        @RequiresApi(29)
        LowLatency(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, true),
    }

    class ActivityListener(private val activity: ComponentActivity) :
            DefaultLifecycleObserver, SharedPreferences.OnSharedPreferenceChangeListener {
        private var keepScreenOn: Boolean = false
            set(value) {
                if (field == value) return
                field = value
                if (value) activity.window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                else activity.window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            }

        init {
            activity.lifecycle.addObserver(this)
            app.pref.registerOnSharedPreferenceChangeListener(this)
            keepScreenOn = mode.keepScreenOn
        }

        override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
            if (key == KEY) keepScreenOn = mode.keepScreenOn
        }

        override fun onDestroy(owner: LifecycleOwner) = app.pref.unregisterOnSharedPreferenceChangeListener(this)
    }

    private val wifi = Services.wifi.createWifiLock(lockType, "vpnhotspot:wifi").apply { acquire() }
    @SuppressLint("WakelockTimeout")
    private val power = service.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "vpnhotspot:power").apply { acquire() }

    override fun close() {
        if (wifi.isHeld) wifi.release()
        if (power.isHeld) power.release()
    }
}
