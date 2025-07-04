package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.BatteryManager
import android.os.Build
import android.os.StatFs
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.TetheringManagerCompat.tetheredIfaces
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiApManager.wifiApState
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.ApiKeyManager
import fi.iki.elonen.NanoHTTPD
import timber.log.Timber
import java.io.BufferedReader
import java.io.File
import java.io.FileReader
import java.io.IOException
import kotlin.math.roundToInt

/**
 * Web服务器类，提供热点控制和状态查询功能
 * 监听9999端口，支持局域网访问
 * 采用API优先架构，支持远程控制
 */
class WebServer(private val context: Context, port: Int = 9999) : NanoHTTPD(port) {
    
    companion object {
        private var instance: WebServer? = null
        private var lastCpuTime: Long = 0
        private var lastCpuIdle: Long = 0
        private var lastCpuUsage: Float = 0.0f
        private var lastCpuCheckTime: Long = 0
        private var cachedSystemStatus: SystemStatus? = null
        private var lastStatusUpdateTime: Long = 0
        private const val STATUS_CACHE_DURATION = 2000L // 2秒缓存
        
        fun getInstance(context: Context): WebServer {
            if (instance == null) {
                instance = WebServer(context.applicationContext)
            }
            return instance!!
        }
        
        fun start(context: Context) {
            val server = getInstance(context)
            if (!server.isAlive) {
                try {
                    server.start()
                    Timber.i("WebServer started on port ${server.listeningPort}")
                } catch (e: IOException) {
                    Timber.e(e, "Failed to start WebServer")
                }
            }
        }
        
        fun stop() {
            instance?.let { server ->
                if (server.isAlive) {
                    server.stop()
                    Timber.i("WebServer stopped")
                }
                instance = null
            }
        }
    }
    
    override fun serve(session: IHTTPSession): Response {
        val uri = session.uri
        val method = session.method
        
        Timber.d("WebServer request: $method $uri")
        
        // favicon.ico 不需要认证
        if (uri == "/favicon.ico") {
            Timber.d("Serving favicon.ico")
            return serveFavicon()
        }
        
        // API路由 - 优先处理API请求
        if (uri.startsWith("/api/")) {
            return handleApiRequest(uri, method, session)
        }
        
        // 检查URL是否包含API Key（格式：/api_key/...）
        val apiKeyFromUrl = extractApiKeyFromUrl(uri)
        if (apiKeyFromUrl != null) {
            // 验证API Key
            if (ApiKeyManager.verifyApiKey(apiKeyFromUrl)) {
                // API Key有效，移除API Key部分并处理剩余路径
                val remainingPath = uri.substringAfter("/$apiKeyFromUrl")
                return when {
                    remainingPath.isEmpty() || remainingPath == "/" -> serveMainPage()
                    else -> serve404()
                }
            } else {
                // API Key无效
                return newFixedLengthResponse(Response.Status.UNAUTHORIZED, "text/html", 
                    """<html><body><h1>401 Unauthorized</h1><p>Invalid API Key</p></body></html>""")
            }
        }
        
        return when {
            uri == "/" -> serveMainPage()
            else -> serve404()
        }
    }
    
    private fun extractApiKeyFromUrl(uri: String): String? {
        // 检查URL格式是否为 /api_key/...
        val segments = uri.split("/").filter { it.isNotEmpty() }
        if (segments.isNotEmpty()) {
            val firstSegment = segments[0]
            // 检查是否是有效的API Key格式（32位十六进制字符串）
            if (firstSegment.length == 32 && firstSegment.all { it.isLetterOrDigit() }) {
                return firstSegment
            }
        }
        return null
    }
    
    private fun handleApiRequest(uri: String, method: Method, session: IHTTPSession): Response {
        // 检查URL是否包含API Key（格式：/api_key/api/...）
        val apiKeyFromUrl = extractApiKeyFromUrl(uri)
        if (apiKeyFromUrl != null) {
            // 验证API Key
            if (!ApiKeyManager.verifyApiKey(apiKeyFromUrl)) {
                return newFixedLengthResponse(Response.Status.UNAUTHORIZED, "application/json", 
                    """{"error": "Unauthorized", "message": "Invalid API Key"}""")
            }
            // API Key有效，移除API Key部分并处理剩余路径
            val remainingPath = uri.substringAfter("/$apiKeyFromUrl")
            return handleApiRequestInternal(remainingPath, method, session)
        }
        
        // API请求需要API Key验证
        if (ApiKeyManager.isApiKeyAuthEnabled()) {
            val authResult = validateApiKeyAuth(session)
            if (!authResult) {
                return newFixedLengthResponse(Response.Status.UNAUTHORIZED, "application/json", 
                    """{"error": "Unauthorized", "message": "Invalid API Key"}""")
            }
        }
        
        return handleApiRequestInternal(uri, method, session)
    }
    
    private fun handleApiRequestInternal(uri: String, method: Method, session: IHTTPSession): Response {
        return when {
            uri == "/api/status" -> serveApiStatus()
            uri == "/api/wifi/start" -> handleApiWifiStart()
            uri == "/api/wifi/stop" -> handleApiWifiStop()
            uri == "/api/system/info" -> serveSystemInfo()
            uri == "/api/generate-key" -> handleGenerateApiKey()
            uri == "/api/toggle-auth" -> handleToggleAuth(session)
            uri == "/api/auth-status" -> handleAuthStatus()
            else -> newFixedLengthResponse(Response.Status.NOT_FOUND, "application/json", 
                """{"error": "Not Found", "message": "API endpoint not found"}""")
        }
    }
    
    private fun validateApiKeyAuth(session: IHTTPSession): Boolean {
        // 检查Authorization header
        val authHeader = session.headers["authorization"]
        if (authHeader != null && authHeader.startsWith("Bearer ")) {
            val apiKey = authHeader.substring(7)
            return ApiKeyManager.verifyApiKey(apiKey)
        }
        
        // 检查URL参数中的api_key
        val apiKey = session.parms["api_key"]
        if (apiKey != null) {
            return ApiKeyManager.verifyApiKey(apiKey)
        }
        
        // 检查X-API-Key header
        val xApiKey = session.headers["x-api-key"]
        if (xApiKey != null) {
            return ApiKeyManager.verifyApiKey(xApiKey)
        }
        
        return false
    }
    
    private fun serveApiStatus(): Response {
        val status = getSystemStatus()
        val json = """
            {
                "success": true,
                "data": {
                    "battery": ${status.battery},
                    "temperature": ${status.temperature},
                    "cpu": ${status.cpu},
                    "wifiStatus": "${status.wifiStatus}",
                    "timestamp": ${System.currentTimeMillis()}
                }
            }
        """.trimIndent()
        
        return newFixedLengthResponse(Response.Status.OK, "application/json", json)
    }
    
    private fun serveSystemInfo(): Response {
        val status = getSystemStatus()
        val json = """
            {
                "success": true,
                "data": {
                    "device": "${Build.MODEL}",
                    "android": "${Build.VERSION.RELEASE}",
                    "battery": ${status.battery},
                    "temperature": ${status.temperature},
                    "cpu": ${status.cpu},
                    "wifiStatus": "${status.wifiStatus}",
                    "timestamp": ${System.currentTimeMillis()}
                }
            }
        """.trimIndent()
        
        return newFixedLengthResponse(Response.Status.OK, "application/json", json)
    }
    
    private fun handleApiWifiStart(): Response {
        return try {
            startWifiTethering()
            val json = """{"success": true, "message": "WiFi热点启动成功"}"""
            newFixedLengthResponse(Response.Status.OK, "application/json", json)
        } catch (e: Exception) {
            Timber.e(e, "Failed to start WiFi tethering")
            val json = """{"success": false, "error": "启动失败: ${e.message}"}"""
            newFixedLengthResponse(Response.Status.INTERNAL_ERROR, "application/json", json)
        }
    }
    
    private fun handleApiWifiStop(): Response {
        return try {
            stopWifiTethering()
            val json = """{"success": true, "message": "WiFi热点已停止"}"""
            newFixedLengthResponse(Response.Status.OK, "application/json", json)
        } catch (e: Exception) {
            Timber.e(e, "Failed to stop WiFi tethering")
            val json = """{"success": false, "error": "停止失败: ${e.message}"}"""
            newFixedLengthResponse(Response.Status.INTERNAL_ERROR, "application/json", json)
        }
    }
    
    private fun handleGenerateApiKey(): Response {
        return try {
            val apiKey = ApiKeyManager.generateApiKey()
            ApiKeyManager.setApiKey(apiKey)
            val json = """{"success": true, "data": {"apiKey": "$apiKey"}}"""
            newFixedLengthResponse(Response.Status.OK, "application/json", json)
        } catch (e: Exception) {
            Timber.e(e, "Failed to generate API key")
            val json = """{"success": false, "error": "生成失败: ${e.message}"}"""
            newFixedLengthResponse(Response.Status.INTERNAL_ERROR, "application/json", json)
        }
    }
    
    private fun handleToggleAuth(session: IHTTPSession): Response {
        return try {
            if (session.method != Method.POST) {
                return newFixedLengthResponse(Response.Status.METHOD_NOT_ALLOWED, "application/json", 
                    """{"success": false, "error": "Method not allowed"}""")
            }
            
            // 解析JSON请求体
            val inputStream = session.inputStream
            val contentLength = session.headers["content-length"]?.toIntOrNull() ?: 0
            val postData = ByteArray(contentLength)
            inputStream.read(postData)
            val postString = String(postData, Charsets.UTF_8)
            
            // 简单的JSON解析
            val enabled = postString.contains("\"enabled\":true")
            
            if (enabled) {
                ApiKeyManager.enableApiKeyAuth()
            } else {
                ApiKeyManager.disableApiKeyAuth()
            }
            
            val json = """{"success": true, "message": "API Key认证已${if (enabled) "启用" else "禁用"}"}"""
            newFixedLengthResponse(Response.Status.OK, "application/json", json)
        } catch (e: Exception) {
            Timber.e(e, "Failed to toggle API key auth")
            val json = """{"success": false, "error": "操作失败: ${e.message}"}"""
            newFixedLengthResponse(Response.Status.INTERNAL_ERROR, "application/json", json)
        }
    }
    
    private fun handleAuthStatus(): Response {
        return try {
            val apiKey = ApiKeyManager.getApiKey() ?: ""
            val enabled = ApiKeyManager.isApiKeyAuthEnabled()
            val json = """{"success": true, "data": {"apiKey": "$apiKey", "enabled": $enabled}}"""
            newFixedLengthResponse(Response.Status.OK, "application/json", json)
        } catch (e: Exception) {
            Timber.e(e, "Failed to get auth status")
            val json = """{"success": false, "error": "获取状态失败: ${e.message}"}"""
            newFixedLengthResponse(Response.Status.INTERNAL_ERROR, "application/json", json)
        }
    }
    
    private fun serveMainPage(): Response {
        val html = """
            <!DOCTYPE html>
            <html>
            <head>
                <meta charset="UTF-8">
                <meta name="viewport" content="width=device-width, initial-scale=1.0">
                <title>VPN热点控制面板</title>
                <style>
                    body { font-family: Arial, sans-serif; margin: 20px; background: #f5f5f5; }
                    .container { max-width: 800px; margin: 0 auto; background: white; padding: 20px; border-radius: 8px; box-shadow: 0 2px 10px rgba(0,0,0,0.1); }
                    .status-card { background: #f8f9fa; padding: 15px; margin: 10px 0; border-radius: 5px; border-left: 4px solid #007bff; }
                    .button { background: #007bff; color: white; padding: 10px 20px; border: none; border-radius: 5px; cursor: pointer; margin: 5px; }
                    .button:hover { background: #0056b3; }
                    .button.danger { background: #dc3545; }
                    .button.danger:hover { background: #c82333; }
                    .button.success { background: #28a745; }
                    .button.success:hover { background: #218838; }
                    .button.info { background: #17a2b8; }
                    .button.info:hover { background: #138496; }
                    .status-indicator { display: inline-block; width: 12px; height: 12px; border-radius: 50%; margin-right: 8px; }
                    .status-on { background: #28a745; }
                    .status-off { background: #dc3545; }
                    .refresh-btn { background: #6c757d; color: white; padding: 5px 10px; border: none; border-radius: 3px; cursor: pointer; margin-left: 10px; }

                </style>
            </head>
            <body>
                <div class="container">
                    <h1>VPN热点控制面板</h1>
                    
                    <div>
                        <div class="status-card">
                            <h3>系统状态 <button class="refresh-btn" onclick="refreshStatus()">刷新</button></h3>
                            <div id="status-content">
                                <p>电量: <span id="battery">加载中...</span></p>
                                <p>温度: <span id="temperature">加载中...</span></p>
                                <p>CPU占用: <span id="cpu">加载中...</span></p>
                                <p>WiFi热点: <span id="wifi-status">加载中...</span></p>
                            </div>
                        </div>
                        
                        <div class="status-card">
                            <h3>热点控制</h3>
                            <button class="button success" onclick="startWifi()">启动WiFi热点</button>
                            <button class="button danger" onclick="stopWifi()">停止WiFi热点</button>
                        </div>
                        
                        <div class="status-card">
                            <h3>API Key管理</h3>
                            <div style="margin-bottom: 15px;">
                                <label>API Key:</label>
                                <input type="text" id="apiKeyInput" readonly style="width: 100%; padding: 8px; margin-top: 5px; border: 1px solid #ddd; border-radius: 4px; background: #f8f9fa;">
                                <button class="button info" onclick="generateApiKey()" style="margin-top: 5px;">生成新API Key</button>
                            </div>
                            <div style="margin-bottom: 15px;">
                                <label style="display: flex; align-items: center; cursor: pointer;">
                                    <input type="checkbox" id="apiKeyEnabled" onchange="toggleApiKeyAuth()" style="margin-right: 8px;">
                                    <span>启用API Key认证</span>
                                </label>
                            </div>
                        </div>
                    

                </div>
                
                <script>

                    
                    function refreshStatus() {
                        fetch('/api/status')
                            .then(response => response.json())
                            .then(data => {
                                if (data.success) {
                                    document.getElementById('battery').textContent = data.data.battery + '%';
                                    document.getElementById('temperature').textContent = data.data.temperature + '°C';
                                    document.getElementById('cpu').textContent = data.data.cpu + '%';
                                    document.getElementById('wifi-status').textContent = data.data.wifiStatus;
                                }
                            })
                            .catch(error => console.error('Error:', error));
                    }
                    
                    function startWifi() {
                        fetch('/api/wifi/start', {method: 'POST'})
                            .then(response => response.json())
                            .then(result => {
                                alert(result.message);
                                refreshStatus();
                            })
                            .catch(error => console.error('Error:', error));
                    }
                    
                    function stopWifi() {
                        fetch('/api/wifi/stop', {method: 'POST'})
                            .then(response => response.json())
                            .then(result => {
                                alert(result.message);
                                refreshStatus();
                            })
                            .catch(error => console.error('Error:', error));
                    }
                    

                    
                    function generateApiKey() {
                        fetch('/api/generate-key', {method: 'POST'})
                            .then(response => response.json())
                            .then(result => {
                                if (result.success) {
                                    document.getElementById('apiKeyInput').value = result.data.apiKey;
                                    alert('API Key已生成');
                                } else {
                                    alert('生成失败: ' + result.error);
                                }
                            })
                            .catch(error => console.error('Error:', error));
                    }
                    
                    function toggleApiKeyAuth() {
                        const enabled = document.getElementById('apiKeyEnabled').checked;
                        fetch('/api/toggle-auth', {
                            method: 'POST',
                            headers: {
                                'Content-Type': 'application/json',
                            },
                            body: JSON.stringify({enabled: enabled})
                        })
                            .then(response => response.json())
                            .then(result => {
                                if (result.success) {
                                    alert(enabled ? 'API Key认证已启用' : 'API Key认证已禁用');
                                } else {
                                    alert('操作失败: ' + result.error);
                                    // 恢复复选框状态
                                    document.getElementById('apiKeyEnabled').checked = !enabled;
                                }
                            })
                            .catch(error => {
                                console.error('Error:', error);
                                alert('操作失败');
                                // 恢复复选框状态
                                document.getElementById('apiKeyEnabled').checked = !enabled;
                            });
                    }
                    
                    function loadApiKeyStatus() {
                        fetch('/api/auth-status')
                            .then(response => response.json())
                            .then(data => {
                                if (data.success) {
                                    document.getElementById('apiKeyInput').value = data.data.apiKey || '';
                                    document.getElementById('apiKeyEnabled').checked = data.data.enabled;
                                }
                            })
                            .catch(error => console.error('Error:', error));
                    }
                    
                    // 页面加载时刷新状态
                    refreshStatus();
                    loadApiKeyStatus();
                    
                    // 每30秒自动刷新状态
                    setInterval(refreshStatus, 30000);
                </script>
            </body>
            </html>
        """.trimIndent()
        
        return newFixedLengthResponse(Response.Status.OK, "text/html; charset=utf-8", html)
    }
    

    
    private fun serveFavicon(): Response {
        Timber.d("serveFavicon called")
        // 返回一个简单的1x1像素的透明PNG图标
        val faviconData = byteArrayOf(
            0x89.toByte(), 0x50.toByte(), 0x4E.toByte(), 0x47.toByte(), 0x0D.toByte(), 0x0A.toByte(), 0x1A.toByte(), 0x0A.toByte(),
            0x00.toByte(), 0x00.toByte(), 0x00.toByte(), 0x0D.toByte(), 0x49.toByte(), 0x48.toByte(), 0x44.toByte(), 0x52.toByte(),
            0x00.toByte(), 0x00.toByte(), 0x00.toByte(), 0x01.toByte(), 0x00.toByte(), 0x00.toByte(), 0x00.toByte(), 0x01.toByte(),
            0x08.toByte(), 0x06.toByte(), 0x00.toByte(), 0x00.toByte(), 0x00.toByte(), 0x1F.toByte(), 0x15.toByte(), 0xC4.toByte(),
            0x89.toByte(), 0x00.toByte(), 0x00.toByte(), 0x00.toByte(), 0x0A.toByte(), 0x49.toByte(), 0x44.toByte(), 0x41.toByte(),
            0x54.toByte(), 0x78.toByte(), 0x9C.toByte(), 0x63.toByte(), 0x00.toByte(), 0x01.toByte(), 0x00.toByte(), 0x00.toByte(),
            0x05.toByte(), 0x00.toByte(), 0x01.toByte(), 0x0D.toByte(), 0x0A.toByte(), 0x2D.toByte(), 0xB4.toByte(), 0x00.toByte(),
            0x00.toByte(), 0x00.toByte(), 0x00.toByte(), 0x49.toByte(), 0x45.toByte(), 0x4E.toByte(), 0x44.toByte(), 0xAE.toByte(),
            0x42.toByte(), 0x60.toByte(), 0x82.toByte()
        )
        Timber.d("Favicon data size: ${faviconData.size}")
        return newFixedLengthResponse(
            Response.Status.OK,
            "image/x-icon",
            faviconData.inputStream(),
            faviconData.size.toLong()
        )
    }
    
    private fun serve404(): Response {
        return newFixedLengthResponse(Response.Status.NOT_FOUND, "text/plain", "404 Not Found")
    }
    
    private fun serveCpuDebug(): Response {
        return try {
            val line = readProcStatWithSu() ?: return newFixedLengthResponse(Response.Status.INTERNAL_ERROR, "text/plain", "Error: su 读取 /proc/stat 失败")
            val parts = line.split("\\s+".toRegex())
            val debugInfo = """
                Raw /proc/stat line: $line
                Parts count: ${parts.size}
                Parts: ${parts.joinToString(", ")}
                \nCurrent values:
                - lastCpuTime: $lastCpuTime
                - lastCpuIdle: $lastCpuIdle
                - lastCpuUsage: $lastCpuUsage
                - lastCpuCheckTime: $lastCpuCheckTime
                \nCurrent calculation:
                - user: ${if (parts.size > 1) parts[1] else "N/A"}
                - nice: ${if (parts.size > 2) parts[2] else "N/A"}
                - system: ${if (parts.size > 3) parts[3] else "N/A"}
                - idle: ${if (parts.size > 4) parts[4] else "N/A"}
                - iowait: ${if (parts.size > 5) parts[5] else "N/A"}
                \nCalculated CPU usage: ${getCpuUsage()}%
            """.trimIndent()
            newFixedLengthResponse(Response.Status.OK, "text/plain", debugInfo)
        } catch (e: Exception) {
            newFixedLengthResponse(Response.Status.INTERNAL_ERROR, "text/plain", "Error: ${e.message}")
        }
    }
    
    private fun startWifiTethering() {
        TetheringManagerCompat.startTethering(
            android.net.TetheringManager.TETHERING_WIFI,
            true,
            object : TetheringManagerCompat.StartTetheringCallback {
                override fun onTetheringStarted() {
                    Timber.i("WiFi tethering started successfully")
                }
                
                override fun onTetheringFailed(error: Int?) {
                    val errorMsg = error?.let { TetheringManagerCompat.tetherErrorLookup(it) } ?: "Unknown error"
                    Timber.w("Failed to start WiFi tethering: $errorMsg")
                    throw RuntimeException("Failed to start WiFi tethering: $errorMsg")
                }
            }
        )
    }
    
    private fun stopWifiTethering() {
        TetheringManagerCompat.stopTethering(android.net.TetheringManager.TETHERING_WIFI)
        Timber.i("WiFi tethering stopped")
    }
    
    private fun getSystemStatus(): SystemStatus {
        val currentTime = System.currentTimeMillis()
        
        // 如果缓存还在有效期内，直接返回缓存的状态
        if (cachedSystemStatus != null && currentTime - lastStatusUpdateTime < STATUS_CACHE_DURATION) {
            return cachedSystemStatus!!
        }
        
        // 更新缓存
        val battery = getBatteryLevel()
        val temperature = getBatteryTemperature()
        val cpu = getCpuUsage()
        val wifiStatus = getWifiStatus()
        
        cachedSystemStatus = SystemStatus(battery, temperature, cpu, wifiStatus)
        lastStatusUpdateTime = currentTime
        
        return cachedSystemStatus!!
    }
    
    private fun getBatteryLevel(): Int {
        val batteryStatus = context.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        val level = batteryStatus?.getIntExtra(BatteryManager.EXTRA_LEVEL, -1) ?: -1
        val scale = batteryStatus?.getIntExtra(BatteryManager.EXTRA_SCALE, -1) ?: -1
        
        return if (level != -1 && scale != -1) {
            (level * 100 / scale.toFloat()).roundToInt()
        } else {
            -1
        }
    }
    
    private fun getBatteryTemperature(): Float {
        val batteryStatus = context.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        val temperature = batteryStatus?.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, -1) ?: -1
        
        return if (temperature != -1) {
            temperature / 10.0f
        } else {
            -1.0f
        }
    }
    
    private fun readProcStatWithSu(): String? {
        return try {
            val process = Runtime.getRuntime().exec(arrayOf("su", "-c", "cat /proc/stat"))
            process.inputStream.bufferedReader().readLine()
        } catch (e: Exception) {
            null
        }
    }
    
    private fun getCpuUsage(): Float {
        return try {
            val currentTime = System.currentTimeMillis()
            if (currentTime - lastCpuCheckTime < 2000) { // 增加缓存时间到2秒
                return lastCpuUsage
            }
            
            // 强制使用su命令读取/proc/stat
            val line = readProcStatWithSu() ?: throw Exception("su命令无法读取 /proc/stat")
            
            val parts = line.split("\\s+".toRegex())
            if (parts.size >= 5) {
                val user = parts[1].toLong()
                val nice = parts[2].toLong()
                val system = parts[3].toLong()
                val idle = parts[4].toLong()
                val iowait = if (parts.size > 5) parts[5].toLong() else 0L
                val currentCpuTime = user + nice + system + idle + iowait
                val currentCpuIdle = idle + iowait
                if (lastCpuTime > 0) {
                    val cpuTimeDiff = currentCpuTime - lastCpuTime
                    val cpuIdleDiff = currentCpuIdle - lastCpuIdle
                    if (cpuTimeDiff > 0) {
                        val cpuUsage = ((cpuTimeDiff - cpuIdleDiff) * 100.0f / cpuTimeDiff).coerceIn(0.0f, 100.0f)
                        lastCpuUsage = cpuUsage
                    }
                } else {
                    lastCpuUsage = 0.0f
                }
                lastCpuTime = currentCpuTime
                lastCpuIdle = currentCpuIdle
                lastCpuCheckTime = currentTime
                lastCpuUsage
            } else {
                0.0f
            }
        } catch (e: Exception) {
            Timber.w(e, "Failed to get CPU usage with su")
            0.0f
        }
    }
    
    private fun getWifiStatus(): String {
        return try {
            val intent = context.registerReceiver(null, IntentFilter(TetheringManagerCompat.ACTION_TETHER_STATE_CHANGED))
            val tetherInterfaces = intent?.tetheredIfaces
            
            val wifiInterfaces = tetherInterfaces?.filter { iface ->
                iface.startsWith("wlan") || iface.startsWith("ap")
            } ?: emptyList()
            
            if (wifiInterfaces.isNotEmpty()) {
                "运行中 (接口: ${wifiInterfaces.joinToString(", ")})"
            } else {
                "已停止"
            }
        } catch (e: Exception) {
            Timber.w(e, "Failed to get WiFi status")
            "未知"
        }
    }
    
    data class SystemStatus(
        val battery: Int,
        val temperature: Float,
        val cpu: Float,
        val wifiStatus: String
    )
} 