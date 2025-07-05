package be.mygod.vpnhotspot.util

import android.content.Context
import android.content.SharedPreferences
import be.mygod.vpnhotspot.OkHttpWebServer
import timber.log.Timber
import java.io.IOException

/**
 * WebServer管理器
 * 负责WebServer的启动、停止和端口配置
 */
object WebServerManager {
    private const val PREFS_NAME = "webserver_prefs"
    private const val KEY_PORT = "port"
    private const val DEFAULT_PORT = 9999
    
    private var prefs: SharedPreferences? = null
    private var currentServer: OkHttpWebServer? = null
    
    fun init(context: Context) {
        if (prefs == null) {
            prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
        }
    }
    
    /**
     * 获取当前端口
     */
    fun getPort(): Int {
        return prefs?.getInt(KEY_PORT, DEFAULT_PORT) ?: DEFAULT_PORT
    }
    
    /**
     * 设置端口
     */
    fun setPort(port: Int) {
        prefs?.edit()?.putInt(KEY_PORT, port)?.apply()
    }
    
    /**
     * 启动WebServer
     */
    fun start(context: Context) {
        val port = getPort()
        
        // 如果当前服务器正在运行且端口不同，先停止
        if (currentServer != null && currentServer!!.isRunning && currentServer!!.port != port) {
            stop()
        }
        
        // 如果服务器未运行，启动新服务器
        if (currentServer == null || !currentServer!!.isRunning) {
            try {
                currentServer = OkHttpWebServer(context.applicationContext, port)
                currentServer!!.start()
                Timber.i("OkHttpWebServer started on port $port")
            } catch (e: IOException) {
                Timber.e(e, "Failed to start OkHttpWebServer on port $port")
                throw e
            }
        }
    }
    
    /**
     * 停止WebServer
     */
    fun stop() {
        currentServer?.let { server ->
            if (server.isRunning) {
                server.stop()
                Timber.i("OkHttpWebServer stopped")
            }
            currentServer = null
        }
    }
    
    /**
     * 重启WebServer
     */
    fun restart(context: Context) {
        stop()
        start(context)
    }
    
    /**
     * 检查WebServer是否正在运行
     */
    fun isRunning(): Boolean {
        return currentServer?.isRunning == true
    }
    
    /**
     * 获取当前监听的端口
     */
    fun getCurrentPort(): Int {
        return currentServer?.port ?: getPort()
    }
} 