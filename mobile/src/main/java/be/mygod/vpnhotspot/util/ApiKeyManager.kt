package be.mygod.vpnhotspot.util

import android.content.Context
import android.content.SharedPreferences
import java.security.SecureRandom
import java.util.*

/**
 * API Key管理工具类
 * 负责生成、验证和管理API Key
 */
object ApiKeyManager {
    private const val PREFS_NAME = "api_key_prefs"
    private const val KEY_API_KEY = "api_key"
    private const val KEY_API_KEY_ENABLED = "api_key_enabled"
    
    private var prefs: SharedPreferences? = null
    
    fun init(context: Context) {
        if (prefs == null) {
            prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
        }
    }
    
    /**
     * 生成新的API Key
     */
    fun generateApiKey(): String {
        val random = SecureRandom()
        val bytes = ByteArray(32)
        random.nextBytes(bytes)
        return Base64.getEncoder().encodeToString(bytes).replace("=", "").replace("+", "-").replace("/", "_")
    }
    
    /**
     * 设置API Key
     */
    fun setApiKey(apiKey: String) {
        prefs?.edit()?.putString(KEY_API_KEY, apiKey)?.apply()
    }
    
    /**
     * 获取当前API Key
     */
    fun getApiKey(): String? {
        return prefs?.getString(KEY_API_KEY, null)
    }
    
    /**
     * 启用API Key认证
     */
    fun enableApiKeyAuth() {
        prefs?.edit()?.putBoolean(KEY_API_KEY_ENABLED, true)?.apply()
    }
    
    /**
     * 禁用API Key认证
     */
    fun disableApiKeyAuth() {
        prefs?.edit()?.putBoolean(KEY_API_KEY_ENABLED, false)?.apply()
    }
    
    /**
     * 检查是否启用了API Key认证
     */
    fun isApiKeyAuthEnabled(): Boolean {
        return prefs?.getBoolean(KEY_API_KEY_ENABLED, false) ?: false
    }
    
    /**
     * 验证API Key
     */
    fun verifyApiKey(apiKey: String): Boolean {
        if (!isApiKeyAuthEnabled()) {
            return true // 如果未启用API Key认证，则允许所有请求
        }
        
        val currentApiKey = getApiKey()
        return currentApiKey != null && currentApiKey == apiKey
    }
    
    /**
     * 检查是否有API Key
     */
    fun hasApiKey(): Boolean {
        return getApiKey() != null
    }
    
    /**
     * 清除API Key
     */
    fun clearApiKey() {
        prefs?.edit()?.remove(KEY_API_KEY)?.apply()
    }
} 