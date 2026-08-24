package be.mygod.vpnhotspot.util

import android.content.Context
import android.content.SharedPreferences
import be.mygod.vpnhotspot.OkHttpWebServer
import timber.log.Timber
import java.io.IOException
import java.net.BindException
import java.net.ServerSocket
import java.util.concurrent.TimeUnit

/**
 * WebServer管理器
 * 负责WebServer的启动、停止和端口配置
 * 增强版本包含端口冲突检测、重试机制和完整的资源清理
 */
object WebServerManager {
    private const val PREFS_NAME = "webserver_prefs"
    private const val KEY_PORT = "port"
    private const val DEFAULT_PORT = 9999
    
    // 备用端口列表，用于端口冲突时的重试
    private val FALLBACK_PORTS = listOf(9999, 10000, 10001, 10002, 10003)
    
    // 资源清理超时时间
    private const val CLEANUP_TIMEOUT_SECONDS = 5L
    
    private var prefs: SharedPreferences? = null
    private var currentServer: OkHttpWebServer? = null
    private var lastUsedPort: Int = DEFAULT_PORT
    
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
     * 启动WebServer，包含端口冲突检测和重试机制
     */
    fun start(context: Context) {
        val preferredPort = getPort()
        
        // 如果当前服务器正在运行且端口不同，先停止
        if (currentServer != null && currentServer!!.isRunning && currentServer!!.port != preferredPort) {
            Timber.i("Stopping current server to change port from ${currentServer!!.port} to $preferredPort")
            stop()
        }
        
        // 如果服务器未运行，启动新服务器
        if (currentServer == null || !currentServer!!.isRunning) {
            startWithPortRetry(context, preferredPort)
        }
    }
    
    /**
     * 使用端口重试机制启动WebServer
     */
    private fun startWithPortRetry(context: Context, preferredPort: Int) {
        val portsToTry = if (preferredPort in FALLBACK_PORTS) {
            // 如果首选端口在备用列表中，将其移到前面
            listOf(preferredPort) + FALLBACK_PORTS.filter { it != preferredPort }
        } else {
            // 如果首选端口不在备用列表中，先尝试首选端口，然后尝试备用端口
            listOf(preferredPort) + FALLBACK_PORTS
        }
        
        var lastException: Exception? = null
        
        for (port in portsToTry) {
            try {
                Timber.d("Attempting to start WebServer on port $port")
                
                // 检查端口是否可用
                if (!isPortAvailable(port)) {
                    Timber.w("Port $port is already in use, trying next port")
                    continue
                }
                
                currentServer = OkHttpWebServer(context.applicationContext, port)
                currentServer!!.start()
                lastUsedPort = port
                
                // 如果使用的端口不是首选端口，更新配置
                if (port != preferredPort) {
                    Timber.i("WebServer started on fallback port $port instead of preferred port $preferredPort")
                    setPort(port)
                } else {
                    Timber.i("WebServer started successfully on preferred port $port")
                }
                
                return // 成功启动，退出方法
                
            } catch (e: BindException) {
                Timber.w(e, "Port $port is in use, trying next port")
                lastException = e
                continue
            } catch (e: IOException) {
                Timber.w(e, "Failed to start WebServer on port $port, trying next port")
                lastException = e
                continue
            } catch (e: Exception) {
                Timber.e(e, "Unexpected error starting WebServer on port $port")
                lastException = e
                continue
            }
        }
        
        // 如果所有端口都失败了，抛出最后一个异常
        val errorMessage = "Failed to start WebServer on any available port. Tried ports: ${portsToTry.joinToString(", ")}"
        Timber.e(lastException, errorMessage)
        throw IOException(errorMessage, lastException)
    }
    
    /**
     * 检查端口是否可用
     */
    private fun isPortAvailable(port: Int): Boolean {
        return try {
            ServerSocket(port).use { 
                true 
            }
        } catch (e: IOException) {
            false
        }
    }
    
    /**
     * 停止WebServer，确保完整的资源清理
     */
    fun stop() {
        currentServer?.let { server ->
            try {
                if (server.isRunning) {
                    Timber.i("Stopping WebServer on port ${server.port}")
                    
                    // 调用服务器的停止方法
                    server.stop()
                    
                    // 等待一小段时间确保资源被释放
                    Thread.sleep(100)
                    
                    Timber.i("WebServer stopped successfully")
                } else {
                    Timber.d("WebServer was already stopped")
                }
            } catch (e: Exception) {
                Timber.e(e, "Error occurred while stopping WebServer")
                // 即使停止过程中出现错误，也要继续清理
            } finally {
                // 确保引用被清除
                currentServer = null
                Timber.d("WebServer reference cleared")
            }
        } ?: run {
            Timber.d("No WebServer instance to stop")
        }
    }
    
    /**
     * 强制停止WebServer，用于紧急情况下的资源清理
     */
    fun forceStop() {
        try {
            Timber.w("Force stopping WebServer")
            currentServer?.let { server ->
                try {
                    // 尝试正常停止
                    server.stop()
                } catch (e: Exception) {
                    Timber.e(e, "Error during force stop")
                }
            }
        } finally {
            currentServer = null
            Timber.i("WebServer force stopped and reference cleared")
        }
    }
    
    /**
     * 重启WebServer，包含完整的错误处理
     */
    fun restart(context: Context) {
        try {
            Timber.i("Restarting WebServer")
            stop()
            
            // 短暂等待确保端口被释放
            Thread.sleep(200)
            
            start(context)
            Timber.i("WebServer restarted successfully")
        } catch (e: Exception) {
            Timber.e(e, "Failed to restart WebServer")
            throw e
        }
    }
    
    /**
     * 检查WebServer是否正在运行
     */
    fun isRunning(): Boolean {
        return try {
            currentServer?.isRunning == true
        } catch (e: Exception) {
            Timber.w(e, "Error checking WebServer running status")
            false
        }
    }
    
    /**
     * 获取当前监听的端口
     */
    fun getCurrentPort(): Int {
        return try {
            currentServer?.port ?: lastUsedPort
        } catch (e: Exception) {
            Timber.w(e, "Error getting current port")
            getPort()
        }
    }
    
    /**
     * 获取WebServer状态信息，用于调试和监控
     */
    fun getStatus(): WebServerStatus {
        return try {
            val server = currentServer
            WebServerStatus(
                isRunning = server?.isRunning ?: false,
                currentPort = server?.port ?: -1,
                configuredPort = getPort(),
                lastUsedPort = lastUsedPort,
                hasServerInstance = server != null
            )
        } catch (e: Exception) {
            Timber.e(e, "Error getting WebServer status")
            WebServerStatus(
                isRunning = false,
                currentPort = -1,
                configuredPort = getPort(),
                lastUsedPort = lastUsedPort,
                hasServerInstance = false,
                error = e.message
            )
        }
    }
    
    /**
     * 清理所有资源，通常在应用关闭时调用
     */
    fun cleanup() {
        try {
            Timber.i("Cleaning up WebServerManager resources")
            forceStop()
            prefs = null
            lastUsedPort = DEFAULT_PORT
            Timber.i("WebServerManager cleanup completed")
        } catch (e: Exception) {
            Timber.e(e, "Error during WebServerManager cleanup")
        }
    }
    
    /**
     * WebServer状态数据类
     */
    data class WebServerStatus(
        val isRunning: Boolean,
        val currentPort: Int,
        val configuredPort: Int,
        val lastUsedPort: Int,
        val hasServerInstance: Boolean,
        val error: String? = null
    )
} 