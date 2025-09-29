package be.mygod.vpnhotspot.util

import android.content.Context
import android.content.SharedPreferences
import androidx.preference.PreferenceManager
import be.mygod.vpnhotspot.App
import timber.log.Timber

object AutoConnectTester {
    
    fun logAutoConnectStatus(context: Context) {
        val settingsPrefs = App.app.pref
        val autoConnectEnabled = settingsPrefs.getBoolean("remote.control.auto.connect", false)
        
        Timber.d("AutoConnectTester: 自动连接状态 = $autoConnectEnabled")
        Timber.d("AutoConnectTester: SharedPreferences文件 = ${settingsPrefs.javaClass.simpleName}")
        
        // 列出所有包含"remote"的键值
        val allPrefs = settingsPrefs.all
        Timber.d("AutoConnectTester: 所有相关设置:")
        allPrefs.forEach { (key, value) ->
            if (key.contains("remote", ignoreCase = true)) {
                Timber.d("AutoConnectTester:   $key = $value")
            }
        }
    }
    
    fun setAutoConnectEnabled(enabled: Boolean) {
        val settingsPrefs = App.app.pref
        settingsPrefs.edit()
            .putBoolean("remote.control.auto.connect", enabled)
            .apply()
        
        Timber.d("AutoConnectTester: 手动设置自动连接为 $enabled")
    }
    
    fun getAutoConnectEnabled(): Boolean {
        val settingsPrefs = App.app.pref
        return settingsPrefs.getBoolean("remote.control.auto.connect", false)
    }
    
    fun resetAutoConnectSetting() {
        val settingsPrefs = App.app.pref
        settingsPrefs.edit()
            .remove("remote.control.auto.connect")
            .apply()
        
        Timber.d("AutoConnectTester: 已重置自动连接设置")
    }
}