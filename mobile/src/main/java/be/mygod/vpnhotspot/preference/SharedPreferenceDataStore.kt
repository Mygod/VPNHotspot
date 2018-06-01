package be.mygod.vpnhotspot.preference

import android.content.SharedPreferences
import android.support.v7.preference.PreferenceDataStore

class SharedPreferenceDataStore(private val pref: SharedPreferences) : PreferenceDataStore() {
    override fun getBoolean(key: String?, defValue: Boolean) = pref.getBoolean(key, defValue)
    override fun getFloat(key: String?, defValue: Float) = pref.getFloat(key, defValue)
    override fun getInt(key: String?, defValue: Int) = pref.getInt(key, defValue)
    override fun getLong(key: String?, defValue: Long) = pref.getLong(key, defValue)
    override fun getString(key: String?, defValue: String?): String? = pref.getString(key, defValue)
    override fun getStringSet(key: String?, defValue: MutableSet<String>?): MutableSet<String>? =
            pref.getStringSet(key, defValue)
    override fun putBoolean(key: String?, value: Boolean) = pref.edit().putBoolean(key, value).apply()
    override fun putFloat(key: String?, value: Float) = pref.edit().putFloat(key, value).apply()
    override fun putInt(key: String?, value: Int) = pref.edit().putInt(key, value).apply()
    override fun putLong(key: String?, value: Long) = pref.edit().putLong(key, value).apply()
    override fun putString(key: String?, value: String?) = pref.edit().putString(key, value).apply()
    override fun putStringSet(key: String?, value: MutableSet<String>?) = pref.edit().putStringSet(key, value).apply()
}
