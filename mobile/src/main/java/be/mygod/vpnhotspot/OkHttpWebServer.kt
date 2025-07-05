package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.BatteryManager
import android.os.Build
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.TetheringManagerCompat.tetheredIfaces
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiApManager.wifiApState
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.ApiKeyManager
import kotlinx.coroutines.*
import okhttp3.*
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.ResponseBody.Companion.toResponseBody
import timber.log.Timber
import java.io.IOException
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.util.concurrent.Executors
import kotlin.math.roundToInt

/**
 * 基于 OkHttp 的高性能异步 Web 服务器
 * 替换 NanoHTTPD，提供更好的性能和异步处理能力
 */
class OkHttpWebServer(private val context: Context, val port: Int = 9999) {
    
    companion object {
        private var instance: OkHttpWebServer? = null
        private var cachedSystemStatus: SystemStatus? = null
        private var lastStatusUpdateTime: Long = 0
        private const val STATUS_CACHE_DURATION = 2000L // 2秒缓存
        
        // CPU使用率计算相关变量
        private var lastCpuTotal = 0L
        private var lastCpuNonIdle = 0L
        
        fun getInstance(context: Context): OkHttpWebServer {
            if (instance == null) {
                instance = OkHttpWebServer(context.applicationContext)
            }
            return instance!!
        }
        
        fun start(context: Context) {
            val server = getInstance(context)
            if (!server.isRunning) {
                try {
                    server.start()
                    Timber.i("OkHttpWebServer started on port ${server.port}")
                } catch (e: IOException) {
                    Timber.e(e, "Failed to start OkHttpWebServer")
                }
            }
        }
        
        fun stop() {
            instance?.let { server ->
                if (server.isRunning) {
                    server.stop()
                    Timber.i("OkHttpWebServer stopped")
                }
                instance = null
            }
        }
    }
    
    private var serverSocket: ServerSocket? = null
    var isRunning = false
        private set
    private val executor = Executors.newCachedThreadPool()
    private val scope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    
    private val client = OkHttpClient.Builder()
        .connectTimeout(30, java.util.concurrent.TimeUnit.SECONDS)
        .readTimeout(30, java.util.concurrent.TimeUnit.SECONDS)
        .writeTimeout(30, java.util.concurrent.TimeUnit.SECONDS)
        .build()
    
    private val jsonMediaType = "application/json; charset=utf-8".toMediaType()
    private val htmlMediaType = "text/html; charset=utf-8".toMediaType()
    private val textMediaType = "text/plain; charset=utf-8".toMediaType()
    
    fun start() {
        if (isRunning) return
        
        try {
            serverSocket = ServerSocket()
            serverSocket?.reuseAddress = true
            serverSocket?.bind(InetSocketAddress(port))
            isRunning = true
            
            scope.launch {
                while (isRunning) {
                    try {
                        val socket = serverSocket?.accept() ?: break
                        handleConnection(socket)
                    } catch (e: IOException) {
                        if (isRunning) {
                            Timber.e(e, "Error accepting connection")
                        }
                    }
                }
            }
            
            Timber.i("OkHttpWebServer started successfully on port $port")
        } catch (e: IOException) {
            Timber.e(e, "Failed to start OkHttpWebServer")
            throw e
        }
    }
    
    fun stop() {
        isRunning = false
        try {
            serverSocket?.close()
            serverSocket = null
            scope.cancel()
            executor.shutdown()
            Timber.i("OkHttpWebServer stopped")
        } catch (e: Exception) {
            Timber.e(e, "Error stopping OkHttpWebServer")
        }
    }
    
    private fun handleConnection(socket: java.net.Socket) {
        executor.execute {
            try {
                val request = parseRequest(socket)
                val response = processRequest(request)
                sendResponse(socket, response)
            } catch (e: Exception) {
                Timber.e(e, "Error handling connection")
                try {
                    sendErrorResponse(socket, 500, "Internal Server Error")
                } catch (ex: Exception) {
                    Timber.e(ex, "Error sending error response")
                }
            } finally {
                try {
                    socket.close()
                } catch (e: Exception) {
                    Timber.e(e, "Error closing socket")
                }
            }
        }
    }
    
    private fun parseRequest(socket: java.net.Socket): HttpRequest {
        val input = socket.getInputStream().bufferedReader()
        val firstLine = input.readLine() ?: throw IOException("Empty request")
        val parts = firstLine.split(" ")
        if (parts.size != 3) throw IOException("Invalid request line: $firstLine")
        
        val method = parts[0]
        val uri = parts[1]
        
        // 读取 headers
        val headers = mutableMapOf<String, String>()
        var line: String?
        while (input.readLine().also { line = it } != null && line!!.isNotEmpty()) {
            val colonIndex = line!!.indexOf(':')
            if (colonIndex > 0) {
                val key = line!!.substring(0, colonIndex).trim().lowercase()
                val value = line!!.substring(colonIndex + 1).trim()
                headers[key] = value
            }
        }
        
        // 读取请求体（如果有）
        var body: String? = null
        val contentLength = headers["content-length"]?.toIntOrNull()
        if (contentLength != null && contentLength > 0) {
            val bodyChars = CharArray(contentLength)
            input.read(bodyChars, 0, contentLength)
            body = String(bodyChars)
        }
        
        return HttpRequest(method, uri, headers, body)
    }
    
    private fun processRequest(request: HttpRequest): HttpResponse {
        val uri = request.uri
        val method = request.method
        
        Timber.d("OkHttpWebServer request: $method $uri")
        
        // favicon.ico 不需要认证
        if (uri == "/favicon.ico") {
            return serveFavicon()
        }
        
        // API路由 - 优先处理API请求
        if (uri.startsWith("/api/")) {
            return handleApiRequest(uri, method, request)
        }
        
        // 检查URL是否包含API Key（格式：/api_key/...）
        val apiKey = extractApiKey(request)
        if (apiKey != null) {
            // 验证API Key
            if (ApiKeyManager.verifyApiKey(apiKey)) {
                // API Key有效，移除API Key部分并处理剩余路径
                val remainingPath = uri.substringAfter("/$apiKey")
                return when {
                    remainingPath.isEmpty() || remainingPath == "/" -> serveMainPage()
                    remainingPath.startsWith("/api/") -> handleApiRequest(remainingPath, method, request)
                    else -> serve404()
                }
            } else {
                // API Key无效
                return HttpResponse(401, jsonMediaType, 
                    """{"error": "Unauthorized", "message": "Invalid API Key"}""")
            }
        }
        
        // 如果没有API Key，返回401
        return HttpResponse(401, jsonMediaType, 
            """{"error": "Unauthorized", "message": "API Key required"}""")
    }
    
    // 统一API Key提取方法
    private fun extractApiKey(request: HttpRequest): String? {
        // 1. URL路径 /api_key/api/xxx
        val segments = request.uri.split("/").filter { it.isNotEmpty() }
        if (segments.isNotEmpty()) {
            val first = segments[0]
            if (first.length >= 16 && first.all { it.isLetterOrDigit() || it == '-' || it == '_' }) {
                return first
            }
        }
        // 2. Header
        val authHeader = request.headers["authorization"]
        if (authHeader != null && authHeader.startsWith("Bearer ")) {
            return authHeader.substring(7)
        }
        val xApiKey = request.headers["x-api-key"]
        if (xApiKey != null) return xApiKey
        // 3. Query参数
        val query = request.uri.substringAfter('?', "")
        if (query.isNotEmpty()) {
            query.split('&').forEach {
                val (k, v) = it.split('=', limit = 2).let { arr -> arr[0] to arr.getOrNull(1) }
                if (k == "api_key" && v != null) return v
            }
        }
        return null
    }
    
    // API认证处理
    private fun handleApiRequest(uri: String, method: String, request: HttpRequest): HttpResponse {
        // 移除查询参数，只保留路径部分
        val path = uri.substringBefore('?')
        
        // 开发者模式API端点（需要开发者模式启用）
        val developerEndpoints = listOf("/api/generate-key", "/api/toggle-auth")
        if (developerEndpoints.contains(path)) {
            if (!ApiKeyManager.isDeveloperModeEnabled()) {
                return HttpResponse(403, jsonMediaType, 
                    """{"error": "Forbidden", "message": "Developer mode required. This API is only available when developer mode is enabled."}""")
            }
            return handleApiRequestInternal(path, method, request)
        }
        
        // 某些API端点不需要认证
        val noAuthEndpoints = listOf("/api/auth-status")
        if (noAuthEndpoints.contains(path)) {
            return handleApiRequestInternal(path, method, request)
        }
        
        // 其他API需要认证
        val apiKey = extractApiKey(request)
        if (apiKey == null || !ApiKeyManager.verifyApiKey(apiKey)) {
            return HttpResponse(401, jsonMediaType, """{"error": "Unauthorized", "message": "Invalid or missing API Key"}""")
        }
        return handleApiRequestInternal(path, method, request)
    }
    
    private fun handleApiRequestInternal(uri: String, method: String, request: HttpRequest): HttpResponse {
        return when {
            uri == "/api/status" -> serveApiStatus()
            uri == "/api/wifi/start" -> handleApiWifiStart()
            uri == "/api/wifi/stop" -> handleApiWifiStop()
            uri == "/api/system/info" -> serveSystemInfo()
            uri == "/api/generate-key" -> handleGenerateApiKey()
            uri == "/api/toggle-auth" -> handleToggleAuth(request)
            uri == "/api/auth-status" -> handleAuthStatus()
            uri == "/api/debug/status" -> serveDebugStatus()
            uri == "/api/test" -> HttpResponse(200, jsonMediaType, """{"test": "ok"}""")
            else -> HttpResponse(404, jsonMediaType, 
                """{"error": "Not Found", "message": "API endpoint not found"}""")
        }
    }
    

    
    private fun serveApiStatus(): HttpResponse {
        val status = getSystemStatus()
        val json = """
            {
                "success": true,
                "data": {
                    "battery": ${status.battery},
                    "batteryTemperature": ${status.batteryTemperature},
                    "cpuTemperature": ${status.cpuTemperature},
                    "cpu": ${status.cpu},
                    "wifiStatus": "${status.wifiStatus}",
                    "timestamp": ${System.currentTimeMillis()}
                }
            }
        """.trimIndent()
        
        return HttpResponse(200, jsonMediaType, json)
    }
    
    private fun serveSystemInfo(): HttpResponse {
        val status = getSystemStatus()
        val json = """
            {
                "success": true,
                "data": {
                    "device": "${Build.MODEL}",
                    "android": "${Build.VERSION.RELEASE}",
                    "battery": ${status.battery},
                    "batteryTemperature": ${status.batteryTemperature},
                    "cpuTemperature": ${status.cpuTemperature},
                    "cpu": ${status.cpu},
                    "wifiStatus": "${status.wifiStatus}",
                    "timestamp": ${System.currentTimeMillis()}
                }
            }
        """.trimIndent()
        
        return HttpResponse(200, jsonMediaType, json)
    }
    
    private fun handleApiWifiStart(): HttpResponse {
        return try {
            startWifiTethering()
            val json = """{"success": true, "message": "WiFi热点启动成功"}"""
            HttpResponse(200, jsonMediaType, json)
        } catch (e: Exception) {
            Timber.e(e, "Failed to start WiFi tethering")
            val json = """{"success": false, "error": "启动失败: ${e.message}"}"""
            HttpResponse(500, jsonMediaType, json)
        }
    }
    
    private fun handleApiWifiStop(): HttpResponse {
        return try {
            stopWifiTethering()
            val json = """{"success": true, "message": "WiFi热点已停止"}"""
            HttpResponse(200, jsonMediaType, json)
        } catch (e: Exception) {
            Timber.e(e, "Failed to stop WiFi tethering")
            val json = """{"success": false, "error": "停止失败: ${e.message}"}"""
            HttpResponse(500, jsonMediaType, json)
        }
    }
    
    private fun handleGenerateApiKey(): HttpResponse {
        return try {
            val apiKey = ApiKeyManager.generateApiKey()
            ApiKeyManager.setApiKey(apiKey)
            val json = """{"success": true, "data": {"apiKey": "$apiKey"}}"""
            HttpResponse(200, jsonMediaType, json)
        } catch (e: Exception) {
            Timber.e(e, "Failed to generate API key")
            val json = """{"success": false, "error": "生成失败: ${e.message}"}"""
            HttpResponse(500, jsonMediaType, json)
        }
    }
    
    private fun handleToggleAuth(request: HttpRequest): HttpResponse {
        return try {
            if (request.method != "POST") {
                return HttpResponse(405, jsonMediaType, 
                    """{"success": false, "error": "Method not allowed"}""")
            }
            
            // 简单的JSON解析
            val enabled = request.body?.contains("\"enabled\":true") == true
            
            if (enabled) {
                ApiKeyManager.enableApiKeyAuth()
            } else {
                ApiKeyManager.disableApiKeyAuth()
            }
            
            val json = """{"success": true, "message": "API Key认证已${if (enabled) "启用" else "禁用"}"}"""
            HttpResponse(200, jsonMediaType, json)
        } catch (e: Exception) {
            Timber.e(e, "Failed to toggle API key auth")
            val json = """{"success": false, "error": "操作失败: ${e.message}"}"""
            HttpResponse(500, jsonMediaType, json)
        }
    }
    
    private fun handleAuthStatus(): HttpResponse {
        return try {
            val apiKey = ApiKeyManager.getApiKey() ?: ""
            val enabled = ApiKeyManager.isApiKeyAuthEnabled()
            val developerMode = ApiKeyManager.isDeveloperModeEnabled()
            val json = """{"success": true, "data": {"apiKey": "$apiKey", "enabled": $enabled, "developerMode": $developerMode}}"""
            HttpResponse(200, jsonMediaType, json)
        } catch (e: Exception) {
            Timber.e(e, "Failed to get auth status")
            val json = """{"success": false, "error": "获取状态失败: ${e.message}"}"""
            HttpResponse(500, jsonMediaType, json)
        }
    }
    
    private fun serveMainPage(): HttpResponse {
        val html = """
            <!DOCTYPE html>
            <html>
            <head>
                <meta charset="UTF-8">
                <meta name="viewport" content="width=device-width, initial-scale=1.0">
                <title>热点控制面板</title>
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
                    .error { color: #dc3545; font-weight: bold; }
                </style>
            </head>
            <body>
                <div class="container">
                    <h1>热点控制面板</h1>
                    
                    <div>
                        <div class="status-card">
                            <h3>系统状态 <button class="refresh-btn" onclick="refreshStatus()">刷新</button></h3>
                            <div id="status-content">
                                <p>电量: <span id="battery">加载中...</span></p>
                                <p>电池温度: <span id="battery-temperature">加载中...</span></p>
                                <p>CPU温度: <span id="cpu-temperature">加载中...</span></p>
                                <p>CPU占用: <span id="cpu">加载中...</span></p>
                                <p>WiFi热点: <span id="wifi-status">加载中...</span></p>
                            </div>
                        </div>
                        
                        <div class="status-card">
                            <h3>热点控制</h3>
                            <button class="button success" onclick="startWifi()">启动WiFi热点</button>
                            <button class="button danger" onclick="stopWifi()">停止WiFi热点</button>
                            <button class="button info" onclick="testApi()">测试API连接</button>
                        </div>
                    </div>
                </div>
                
                <script>
                    // 获取当前页面的API Key（从URL路径中提取）
                    function getApiKey() {
                        const pathSegments = window.location.pathname.split('/').filter(segment => segment.length > 0);
                        console.log('Path segments:', pathSegments);
                        const apiKey = pathSegments.length > 0 ? pathSegments[0] : null;
                        console.log('Extracted API Key:', apiKey);
                        return apiKey;
                    }
                    
                    // 构建API URL
                    function getApiUrl(endpoint) {
                        const apiKey = getApiKey();
                        const url = apiKey ? '/' + apiKey + endpoint : endpoint;
                        console.log('API URL:', url);
                        return url;
                    }
                    
                    function refreshStatus() {
                        try {
                            const apiUrl = getApiUrl('/api/status');
                            console.log('Refreshing status from:', apiUrl);
                            
                            // 显示加载状态
                            document.getElementById('battery').textContent = '加载中...';
                            document.getElementById('battery-temperature').textContent = '加载中...';
                            document.getElementById('cpu-temperature').textContent = '加载中...';
                            document.getElementById('cpu').textContent = '加载中...';
                            document.getElementById('wifi-status').textContent = '加载中...';
                            
                            fetch(apiUrl, {
                                method: 'GET',
                                headers: {
                                    'Accept': 'application/json',
                                    'Content-Type': 'application/json'
                                }
                            })
                            .then(response => {
                                console.log('Response status:', response.status);
                                console.log('Response headers:', response.headers);
                                if (!response.ok) {
                                    throw new Error('HTTP ' + response.status + ': ' + response.statusText);
                                }
                                return response.json();
                            })
                            .then(data => {
                                console.log('Response data:', data);
                                if (data.success) {
                                    document.getElementById('battery').textContent = data.data.battery + '%';
                                    
                                    // 电池温度
                                    if (data.data.batteryTemperature === -1 || data.data.batteryTemperature === "-1") {
                                        document.getElementById('battery-temperature').textContent = '无法获取';
                                    } else {
                                        document.getElementById('battery-temperature').textContent = data.data.batteryTemperature + '°C';
                                    }
                                    
                                    // CPU温度
                                    if (data.data.cpuTemperature === -1 || data.data.cpuTemperature === "-1") {
                                        document.getElementById('cpu-temperature').textContent = '无法获取';
                                    } else {
                                        document.getElementById('cpu-temperature').textContent = data.data.cpuTemperature + '°C';
                                    }
                                    
                                    // CPU使用率
                                    if (data.data.cpu === -1 || data.data.cpu === "-1") {
                                        document.getElementById('cpu').textContent = '需要root权限';
                                    } else {
                                        document.getElementById('cpu').textContent = data.data.cpu + '%';
                                    }
                                    
                                    document.getElementById('wifi-status').textContent = data.data.wifiStatus;
                                } else {
                                    throw new Error(data.error || '获取状态失败');
                                }
                            })
                            .catch(error => {
                                console.error('Error refreshing status:', error);
                                document.getElementById('battery').textContent = '加载失败: ' + error.message;
                                document.getElementById('battery-temperature').textContent = '加载失败: ' + error.message;
                                document.getElementById('cpu-temperature').textContent = '加载失败: ' + error.message;
                                document.getElementById('cpu').textContent = '加载失败: ' + error.message;
                                document.getElementById('wifi-status').textContent = '加载失败: ' + error.message;
                            });
                        } catch (error) {
                            console.error('Error in refreshStatus:', error);
                            document.getElementById('battery').textContent = 'JavaScript错误: ' + error.message;
                            document.getElementById('battery-temperature').textContent = 'JavaScript错误: ' + error.message;
                            document.getElementById('cpu-temperature').textContent = 'JavaScript错误: ' + error.message;
                            document.getElementById('cpu').textContent = 'JavaScript错误: ' + error.message;
                            document.getElementById('wifi-status').textContent = 'JavaScript错误: ' + error.message;
                        }
                    }
                    
                    function startWifi() {
                        try {
                            const apiUrl = getApiUrl('/api/wifi/start');
                            console.log('Starting WiFi from:', apiUrl);
                            
                            fetch(apiUrl, {
                                method: 'POST',
                                headers: {
                                    'Accept': 'application/json',
                                    'Content-Type': 'application/json'
                                }
                            })
                            .then(response => {
                                console.log('Start response status:', response.status);
                                return response.json();
                            })
                            .then(result => {
                                console.log('Start response data:', result);
                                if (result.success) {
                                    alert(result.message);
                                } else {
                                    alert('启动失败: ' + (result.error || '未知错误'));
                                }
                                refreshStatus();
                            })
                            .catch(error => {
                                console.error('Start error:', error);
                                alert('启动失败: ' + error.message);
                            });
                        } catch (error) {
                            console.error('Error in startWifi:', error);
                            alert('JavaScript错误: ' + error.message);
                        }
                    }
                    
                    function stopWifi() {
                        try {
                            const apiUrl = getApiUrl('/api/wifi/stop');
                            console.log('Stopping WiFi from:', apiUrl);
                            
                            fetch(apiUrl, {
                                method: 'POST',
                                headers: {
                                    'Accept': 'application/json',
                                    'Content-Type': 'application/json'
                                }
                            })
                            .then(response => {
                                console.log('Stop response status:', response.status);
                                return response.json();
                            })
                            .then(result => {
                                console.log('Stop response data:', result);
                                if (result.success) {
                                    alert(result.message);
                                } else {
                                    alert('停止失败: ' + (result.error || '未知错误'));
                                }
                                refreshStatus();
                            })
                            .catch(error => {
                                console.error('Stop error:', error);
                                alert('停止失败: ' + error.message);
                            });
                        } catch (error) {
                            console.error('Error in stopWifi:', error);
                            alert('JavaScript错误: ' + error.message);
                        }
                    }
                    
                    function testApi() {
                        const apiUrl = getApiUrl('/api/status');
                        console.log('Testing API:', apiUrl);
                        alert('正在测试API: ' + apiUrl);
                        
                        fetch(apiUrl)
                            .then(response => {
                                console.log('Test response:', response);
                                return response.text();
                            })
                            .then(text => {
                                console.log('Test response text:', text);
                                alert('API响应: ' + text.substring(0, 100) + '...');
                            })
                            .catch(error => {
                                console.error('Test error:', error);
                                alert('API测试失败: ' + error.message);
                            });
                    }
                    
                    // 页面加载时显示调试信息
                    console.log('Page loaded, current URL:', window.location.href);
                    console.log('Current pathname:', window.location.pathname);
                    
                    // 页面加载时刷新状态
                    refreshStatus();
                    
                    // 每30秒自动刷新状态
                    setInterval(refreshStatus, 30000);
                </script>
            </body>
            </html>
        """.trimIndent()
        
        return HttpResponse(200, htmlMediaType, html)
    }
    
    private fun serveFavicon(): HttpResponse {
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
        return HttpResponse(200, "image/x-icon".toMediaType(), String(faviconData, Charsets.ISO_8859_1))
    }
    
    private fun serve404(): HttpResponse {
        return HttpResponse(404, textMediaType, "404 Not Found")
    }
    
    private fun serveDebugStatus(): HttpResponse {
        return try {
            val battery = getBatteryLevel()
            val batteryTemperature = getBatteryTemperature()
            val cpuTemperature = getCpuTemperature()
            val cpu = getCpuUsage()
            val wifiStatus = getWifiStatus()
            val cpuText = if (cpu == -1f) "需要root权限/无权限" else "$cpu%"
            val batteryTempText = if (batteryTemperature == -1f) "无法获取" else "${batteryTemperature}°C"
            val cpuTempText = if (cpuTemperature == -1f) "无法获取" else "${cpuTemperature}°C"
            val debugInfo = """
                系统状态调试信息:
                
                电量获取:
                - 电量: $battery%
                - 电池温度: $batteryTempText
                
                CPU获取:
                - CPU使用率: $cpuText
                - CPU温度: $cpuTempText
                
                WiFi状态:
                - 状态: $wifiStatus
                
                缓存信息:
                - 缓存状态: ${if (cachedSystemStatus != null) "已缓存" else "未缓存"}
                - 最后更新时间: $lastStatusUpdateTime
                - 当前时间: ${System.currentTimeMillis()}
            """.trimIndent()
            HttpResponse(200, textMediaType, debugInfo)
        } catch (e: Exception) {
            HttpResponse(500, textMediaType, "调试错误: ${e.message}")
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
        
        // 更新缓存 - 获取实时状态
        val battery = getBatteryLevel()
        val batteryTemperature = getBatteryTemperature()
        val cpuTemperature = getCpuTemperature()
        val cpu = getCpuUsage()
        val wifiStatus = getWifiStatus()
        
        cachedSystemStatus = SystemStatus(battery, batteryTemperature, cpuTemperature, cpu, wifiStatus)
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
    
    private fun getCpuTemperature(): Float {
        return try {
            // 尝试多个可能的CPU温度传感器路径
            val thermalPaths = listOf(
                "/sys/class/thermal/thermal_zone0/temp",
                "/sys/class/thermal/thermal_zone1/temp",
                "/sys/class/thermal/thermal_zone2/temp",
                "/sys/devices/virtual/thermal/thermal_zone0/temp",
                "/sys/devices/virtual/thermal/thermal_zone1/temp",
                "/proc/mtktscpu/mtktscpu"
            )
            
            for (path in thermalPaths) {
                try {
                    val process = Runtime.getRuntime().exec(arrayOf("su", "-c", "cat $path"))
                    val completed = process.waitFor(2, java.util.concurrent.TimeUnit.SECONDS)
                    
                    if (completed) {
                        val reader = process.inputStream.bufferedReader()
                        val tempStr = reader.readLine()?.trim()
                        reader.close()
                        process.waitFor()
                        
                        if (tempStr != null && tempStr.isNotEmpty()) {
                            val temp = tempStr.toIntOrNull()
                            if (temp != null) {
                                // 大多数传感器返回毫摄氏度，需要除以1000
                                val celsius = if (temp > 1000) temp / 1000.0f else temp.toFloat()
                                // 合理的CPU温度范围应该在20-100°C之间
                                if (celsius in 20.0f..100.0f) {
                                    return celsius
                                }
                            }
                        }
                    } else {
                        process.destroy()
                    }
                } catch (e: Exception) {
                    // 继续尝试下一个路径
                    continue
                }
            }
            
            // 如果所有路径都失败，返回-1
            -1.0f
            
        } catch (e: Exception) {
            Timber.w(e, "Failed to get CPU temperature")
            -1.0f
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
    
    private fun getCpuUsage(): Float {
        return try {
            // 使用su命令读取/proc/stat，设置超时避免阻塞
            val process = Runtime.getRuntime().exec(arrayOf("su", "-c", "cat /proc/stat"))
            val completed = process.waitFor(3, java.util.concurrent.TimeUnit.SECONDS)
            
            if (!completed) {
                process.destroy()
                Timber.w("CPU usage calculation timeout")
                return -1f
            }
            
            val reader = process.inputStream.bufferedReader()
            val firstLine = reader.readLine()
            reader.close()
            process.waitFor()
            
            if (firstLine == null || !firstLine.startsWith("cpu ")) {
                Timber.w("Invalid /proc/stat format")
                return -1f
            }
            
            // 解析CPU时间
            val parts = firstLine.split("\\s+".toRegex())
            if (parts.size < 5) {
                Timber.w("Insufficient CPU stats data")
                return -1f
            }
            
            val user = parts[1].toLong()
            val nice = parts[2].toLong()
            val system = parts[3].toLong()
            val idle = parts[4].toLong()
            
            val currentTotal = user + nice + system + idle
            val currentNonIdle = user + nice + system
            
            // 获取上次的CPU时间
            val lastTotal = lastCpuTotal
            val lastNonIdle = lastCpuNonIdle
            
            // 计算差值
            val totalDiff = currentTotal - lastTotal
            val nonIdleDiff = currentNonIdle - lastNonIdle
            
            // 更新上次的值
            lastCpuTotal = currentTotal
            lastCpuNonIdle = currentNonIdle
            
            // 如果是第一次调用，返回0
            if (lastTotal == 0L) {
                return 0f
            }
            
            // 计算CPU使用率
            val cpuUsage = if (totalDiff > 0) {
                (nonIdleDiff.toFloat() / totalDiff.toFloat()) * 100f
            } else {
                0f
            }
            
            // 限制精度到小数点后1位
            (cpuUsage * 10).roundToInt() / 10f
            
        } catch (e: Exception) {
            Timber.w(e, "Failed to get CPU usage")
            -1f
        }
    }
    
    private fun sendResponse(socket: java.net.Socket, response: HttpResponse) {
        val output = socket.getOutputStream().bufferedWriter()
        val bodyBytes = response.body.toByteArray(response.contentType.charset() ?: Charsets.UTF_8)
        output.write("HTTP/1.1 ${response.statusCode} ${getStatusText(response.statusCode)}\r\n")
        output.write("Content-Type: ${response.contentType}\r\n")
        output.write("Content-Length: ${bodyBytes.size}\r\n")
        output.write("Access-Control-Allow-Origin: *\r\n")
        output.write("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n")
        output.write("Access-Control-Allow-Headers: Content-Type, Accept, Authorization, X-API-Key\r\n")
        output.write("Connection: close\r\n")
        output.write("\r\n")
        output.write(response.body)
        output.flush()
    }
    
    private fun sendErrorResponse(socket: java.net.Socket, statusCode: Int, message: String) {
        val response = HttpResponse(statusCode, textMediaType, message)
        sendResponse(socket, response)
    }
    
    private fun getStatusText(statusCode: Int): String {
        return when (statusCode) {
            200 -> "OK"
            401 -> "Unauthorized"
            404 -> "Not Found"
            405 -> "Method Not Allowed"
            500 -> "Internal Server Error"
            else -> "Unknown"
        }
    }
    
    data class HttpRequest(
        val method: String,
        val uri: String,
        val headers: Map<String, String>,
        val body: String? = null
    )
    
    data class HttpResponse(
        val statusCode: Int,
        val contentType: MediaType,
        val body: String
    )
    
    data class SystemStatus(
        val battery: Int,
        val batteryTemperature: Float,
        val cpuTemperature: Float,
        val cpu: Float,
        val wifiStatus: String
    )
}