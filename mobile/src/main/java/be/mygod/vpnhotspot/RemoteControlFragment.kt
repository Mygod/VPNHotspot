package be.mygod.vpnhotspot

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.os.Bundle
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.Toast
import androidx.fragment.app.Fragment
import androidx.lifecycle.lifecycleScope
import be.mygod.vpnhotspot.databinding.FragmentRemoteControlBinding
import be.mygod.vpnhotspot.util.ApiKeyManager
import be.mygod.vpnhotspot.util.WebServerManager
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import timber.log.Timber
import java.net.HttpURLConnection
import java.net.URL
import java.net.URLEncoder

class RemoteControlFragment : Fragment() {
    private var _binding: FragmentRemoteControlBinding? = null
    private val binding get() = _binding!!
    
    private lateinit var prefs: SharedPreferences
    
    companion object {
        private const val PREFS_NAME = "remote_control_prefs"
        private const val KEY_LAST_IP = "last_ip"
        private const val KEY_LAST_PORT = "last_port"
        private const val KEY_LAST_API_KEY = "last_api_key"
        private const val KEY_AUTO_CONNECT = "auto_connect"
        private const val KEY_MANUAL_MODIFIED = "manual_modified"
    }

    override fun onCreateView(
        inflater: LayoutInflater,
        container: ViewGroup?,
        savedInstanceState: Bundle?
    ): View {
        _binding = FragmentRemoteControlBinding.inflate(inflater, container, false)
        return binding.root
    }

    override fun onViewCreated(view: View, savedInstanceState: Bundle?) {
        super.onViewCreated(view, savedInstanceState)
        
        // 初始化SharedPreferences
        prefs = requireContext().getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
        
        setupUI()
        setupListeners()
        loadLastConnectionInfo()
    }

    private fun setupUI() {
        // 设置初始状态
        binding.remoteStatusCard.visibility = View.GONE
        binding.progressBar.visibility = View.GONE
    }
    
    private fun loadLastConnectionInfo() {
        // 检查是否已经手动修改过
        val manualModified = prefs.getBoolean(KEY_MANUAL_MODIFIED, false)
        
        if (manualModified) {
            // 如果已经手动修改过，加载保存的连接信息
            val lastIp = prefs.getString(KEY_LAST_IP, null)
            val lastPort = prefs.getInt(KEY_LAST_PORT, 9999)
            val lastApiKey = prefs.getString(KEY_LAST_API_KEY, null)
            val autoConnect = prefs.getBoolean(KEY_AUTO_CONNECT, false)
            
            if (lastIp != null && lastApiKey != null) {
                binding.ipInput.setText(lastIp)
                binding.portInput.setText(lastPort.toString())
                binding.passwordInput.setText(lastApiKey)
                
                // 如果设置了自动连接，则自动连接
                if (autoConnect) {
                    connectToRemoteDevice()
                }
            } else {
                // 如果没有保存的信息，使用默认地址
                loadDefaultLocalAddress()
            }
        } else {
            // 如果没有手动修改过，使用默认的本地地址
            loadDefaultLocalAddress()
        }
    }
    
    private fun loadDefaultLocalAddress() {
        // 获取本地IP地址
        val localIp = getDeviceIpAddress()
        val localPort = WebServerManager.getPort()
        val localApiKey = ApiKeyManager.getApiKey()
        
        if (localIp != null && !localApiKey.isNullOrEmpty()) {
            binding.ipInput.setText(localIp)
            binding.portInput.setText(localPort.toString())
            binding.passwordInput.setText(localApiKey)
            Toast.makeText(context, "已加载本地设备地址", Toast.LENGTH_SHORT).show()
        } else {
            // 如果无法获取本地信息，设置默认值
            binding.ipInput.setText("192.168.1.1")
            binding.portInput.setText("9999")
            binding.passwordInput.setText("default_api_key_for_debug_2024")
        }
    }
    
    private fun getDeviceIpAddress(): String? {
        return try {
            // 首先尝试获取静态IP设置
            val staticIps = StaticIpSetter.ips
            if (staticIps.isNotEmpty()) {
                // 解析静态IP设置，可能包含多个IP（每行一个）
                val ipLines = staticIps.lines().filter { it.isNotEmpty() }
                for (ipLine in ipLines) {
                    val ip = ipLine.trim()
                    // 检查是否是有效的IPv4地址
                    if (isValidIPv4(ip)) {
                        return ip
                    }
                }
            }
            
            // 如果没有设置静态IP或静态IP无效，则获取网络接口的IP
            val networkInterfaces = java.net.NetworkInterface.getNetworkInterfaces()
            while (networkInterfaces.hasMoreElements()) {
                val networkInterface = networkInterfaces.nextElement()
                
                // 跳过回环接口和未启用的接口
                if (networkInterface.isLoopback || !networkInterface.isUp) {
                    continue
                }
                
                // 获取接口的IP地址
                val inetAddresses = networkInterface.inetAddresses
                while (inetAddresses.hasMoreElements()) {
                    val inetAddress = inetAddresses.nextElement()
                    
                    // 只选择IPv4地址，排除回环地址
                    if (inetAddress is java.net.Inet4Address && !inetAddress.isLoopbackAddress) {
                        val ip = inetAddress.hostAddress
                        
                        // 返回任何有效的IPv4地址（不限于私有IP）
                        if (ip != null) {
                            return ip
                        }
                    }
                }
            }
            
            null
        } catch (e: Exception) {
            Timber.e(e, "Failed to get device IP address")
            null
        }
    }
    
    private fun isValidIPv4(ip: String): Boolean {
        return try {
            val parts = ip.split(".")
            if (parts.size != 4) return false
            
            for (part in parts) {
                val num = part.toInt()
                if (num < 0 || num > 255) return false
            }
            true
        } catch (e: Exception) {
            false
        }
    }
    
    private fun saveConnectionInfo(ip: String, port: Int, apiKey: String) {
        prefs.edit()
            .putString(KEY_LAST_IP, ip)
            .putInt(KEY_LAST_PORT, port)
            .putString(KEY_LAST_API_KEY, apiKey)
            .putBoolean(KEY_AUTO_CONNECT, true)
            .putBoolean(KEY_MANUAL_MODIFIED, true) // 标记为手动修改
            .apply()
    }
    
    private fun markAsManuallyModified() {
        prefs.edit()
            .putBoolean(KEY_MANUAL_MODIFIED, true)
            .apply()
    }

    private fun setupListeners() {
        binding.connectButton.setOnClickListener {
            connectToRemoteDevice()
        }

        binding.startWifiButton.setOnClickListener {
            remoteStartWifi()
        }

        binding.stopWifiButton.setOnClickListener {
            remoteStopWifi()
        }

        binding.refreshButton.setOnClickListener {
            refreshRemoteStatus()
        }

        binding.scanButton.setOnClickListener {
            startQRCodeScanner()
        }
        
        // 添加测试连接按钮的长按监听器来显示详细信息
        binding.connectButton.setOnLongClickListener {
            testConnectionDetails()
            true
        }
        
        // 监听输入框的变化，如果用户手动修改了输入框，标记为手动修改
        binding.ipInput.setOnFocusChangeListener { _, hasFocus ->
            if (!hasFocus) {
                markAsManuallyModified()
            }
        }
        
        binding.portInput.setOnFocusChangeListener { _, hasFocus ->
            if (!hasFocus) {
                markAsManuallyModified()
            }
        }
        
        binding.passwordInput.setOnFocusChangeListener { _, hasFocus ->
            if (!hasFocus) {
                markAsManuallyModified()
            }
        }
    }

    private fun connectToRemoteDevice() {
        val ip = binding.ipInput.text.toString().trim()
        val port = binding.portInput.text.toString().trim().toIntOrNull() ?: 9999
        val apiKey = binding.passwordInput.text.toString().trim()

        if (ip.isEmpty() || apiKey.isEmpty()) {
            Toast.makeText(context, "请输入IP地址和API Key", Toast.LENGTH_SHORT).show()
            return
        }

        binding.progressBar.visibility = View.VISIBLE
        binding.connectButton.isEnabled = false

        lifecycleScope.launch {
            try {
                val result = withContext(Dispatchers.IO) {
                    fetchRemoteDeviceInfo(ip, port, apiKey)
                }

                if (result.success && result.data != null) {
                    displayRemoteStatus(result.data)
                    binding.remoteStatusCard.visibility = View.VISIBLE
                    // 保存连接信息
                    binding.ipInput.tag = ip
                    binding.portInput.tag = port
                    binding.passwordInput.tag = apiKey
                    // 保存到SharedPreferences
                    saveConnectionInfo(ip, port, apiKey)
                    Toast.makeText(context, "连接成功", Toast.LENGTH_SHORT).show()
                } else {
                    Toast.makeText(context, "连接失败: ${result.error}", Toast.LENGTH_SHORT).show()
                }
            } catch (e: Exception) {
                Toast.makeText(context, "连接失败: ${e.message}", Toast.LENGTH_SHORT).show()
            } finally {
                binding.progressBar.visibility = View.GONE
                binding.connectButton.isEnabled = true
            }
        }
    }

    private fun refreshRemoteStatus() {
        val ip = binding.ipInput.tag as? String
        val port = binding.portInput.tag as? Int ?: 9999
        val apiKey = binding.passwordInput.tag as? String

        if (ip == null || apiKey == null) {
            Toast.makeText(context, "请先连接远程设备", Toast.LENGTH_SHORT).show()
            return
        }

        binding.progressBar.visibility = View.VISIBLE

        lifecycleScope.launch {
            try {
                val result = withContext(Dispatchers.IO) {
                    fetchRemoteDeviceInfo(ip, port, apiKey)
                }

                if (result.success && result.data != null) {
                    displayRemoteStatus(result.data)
                    Toast.makeText(context, "状态已刷新", Toast.LENGTH_SHORT).show()
                } else {
                    Toast.makeText(context, "刷新失败: ${result.error}", Toast.LENGTH_SHORT).show()
                }
            } catch (e: Exception) {
                Toast.makeText(context, "刷新失败: ${e.message}", Toast.LENGTH_SHORT).show()
            } finally {
                binding.progressBar.visibility = View.GONE
            }
        }
    }

    private fun remoteStartWifi() {
        val ip = binding.ipInput.tag as? String
        val port = binding.portInput.tag as? Int ?: 9999
        val apiKey = binding.passwordInput.tag as? String

        if (ip == null || apiKey == null) {
            Toast.makeText(context, "请先连接远程设备", Toast.LENGTH_SHORT).show()
            return
        }

        binding.progressBar.visibility = View.VISIBLE

        lifecycleScope.launch {
            try {
                val result = withContext(Dispatchers.IO) {
                    remoteWifiControl(ip, port, apiKey, true)
                }

                if (result.success) {
                    Toast.makeText(context, result.message, Toast.LENGTH_SHORT).show()
                    refreshRemoteStatus()
                } else {
                    Toast.makeText(context, "启动失败: ${result.error}", Toast.LENGTH_SHORT).show()
                }
            } catch (e: Exception) {
                Toast.makeText(context, "启动失败: ${e.message}", Toast.LENGTH_SHORT).show()
            } finally {
                binding.progressBar.visibility = View.GONE
            }
        }
    }

    private fun remoteStopWifi() {
        val ip = binding.ipInput.tag as? String
        val port = binding.portInput.tag as? Int ?: 9999
        val apiKey = binding.passwordInput.tag as? String

        if (ip == null || apiKey == null) {
            Toast.makeText(context, "请先连接远程设备", Toast.LENGTH_SHORT).show()
            return
        }

        binding.progressBar.visibility = View.VISIBLE

        lifecycleScope.launch {
            try {
                val result = withContext(Dispatchers.IO) {
                    remoteWifiControl(ip, port, apiKey, false)
                }

                if (result.success) {
                    Toast.makeText(context, result.message, Toast.LENGTH_SHORT).show()
                    refreshRemoteStatus()
                } else {
                    Toast.makeText(context, "停止失败: ${result.error}", Toast.LENGTH_SHORT).show()
                }
            } catch (e: Exception) {
                Toast.makeText(context, "停止失败: ${e.message}", Toast.LENGTH_SHORT).show()
            } finally {
                binding.progressBar.visibility = View.GONE
            }
        }
    }

    private fun displayRemoteStatus(data: JSONObject) {
        binding.deviceName.text = data.optString("device", "未知设备")
        binding.batteryLevel.text = "${data.optInt("battery", 0)}%"
        
        // 优先显示电池温度，如果没有则显示旧的temperature字段
        val batteryTemp = data.optDouble("batteryTemperature", -1.0)
        val oldTemp = data.optDouble("temperature", -1.0)
        val displayTemp = if (batteryTemp != -1.0) batteryTemp else oldTemp
        binding.temperature.text = if (displayTemp != -1.0) "${displayTemp}°C" else "无法获取"
        
        binding.cpuUsage.text = "${data.optDouble("cpu", 0.0)}%"
        binding.wifiStatus.text = data.optString("wifiStatus", "未知")
        
        println("RemoteControl: 显示状态 - 设备: ${binding.deviceName.text}, 电量: ${binding.batteryLevel.text}, 温度: ${binding.temperature.text}, CPU: ${binding.cpuUsage.text}, WiFi: ${binding.wifiStatus.text}")
    }

    private fun testConnectionDetails() {
        val ip = binding.ipInput.text.toString().trim()
        val port = binding.portInput.text.toString().trim().toIntOrNull() ?: 9999
        val apiKey = binding.passwordInput.text.toString().trim()

        if (ip.isEmpty() || apiKey.isEmpty()) {
            Toast.makeText(context, "请输入IP地址和API Key", Toast.LENGTH_SHORT).show()
            return
        }

        binding.progressBar.visibility = View.VISIBLE
        binding.connectButton.isEnabled = false

        lifecycleScope.launch {
            try {
                // 测试多个端点
                val testResults = mutableListOf<String>()
                
                // 1. 测试基本连接
                testResults.add("=== 连接测试 ===")
                testResults.add("目标: http://$ip:$port/$apiKey")
                
                // 2. 测试状态API
                testResults.add("\n=== 状态API测试 ===")
                val statusResult = withContext(Dispatchers.IO) {
                    fetchRemoteDeviceInfo(ip, port, apiKey)
                }
                testResults.add("状态API: ${if (statusResult.success) "成功" else "失败 - ${statusResult.error}"}")
                if (statusResult.data != null) {
                    testResults.add("数据: ${statusResult.data}")
                }
                
                // 3. 测试WiFi控制API (不实际执行，只测试连通性)
                testResults.add("\n=== WiFi API连通性测试 ===")
                val wifiTestResult = withContext(Dispatchers.IO) {
                    testWifiApiConnectivity(ip, port, apiKey)
                }
                testResults.add("WiFi API: ${if (wifiTestResult.success) "可访问" else "失败 - ${wifiTestResult.error}"}")
                
                // 显示测试结果
                val resultText = testResults.joinToString("\n")
                Toast.makeText(context, "测试完成，查看日志获取详细信息", Toast.LENGTH_LONG).show()
                println("RemoteControl: 详细测试结果:\n$resultText")
                
            } catch (e: Exception) {
                Toast.makeText(context, "测试失败: ${e.message}", Toast.LENGTH_SHORT).show()
                println("RemoteControl: 测试异常: ${e.message}")
                e.printStackTrace()
            } finally {
                binding.progressBar.visibility = View.GONE
                binding.connectButton.isEnabled = true
            }
        }
    }
    
    private suspend fun testWifiApiConnectivity(ip: String, port: Int, apiKey: String): ApiResult {
        return try {
            // 只测试URL的可访问性，不实际发送POST请求
            val url = URL("http://$ip:$port/$apiKey/api/wifi/start")
            val connection = url.openConnection() as HttpURLConnection
            connection.requestMethod = "GET" // 使用GET测试连通性
            connection.connectTimeout = 5000
            connection.readTimeout = 5000
            
            val responseCode = connection.responseCode
            connection.disconnect()
            
            // 即使返回405 Method Not Allowed也说明端点可访问
            if (responseCode in 200..499) {
                ApiResult(true, null, null, "端点可访问 (HTTP $responseCode)")
            } else {
                ApiResult(false, null, "HTTP $responseCode", null)
            }
        } catch (e: Exception) {
            ApiResult(false, null, "连接失败", e.message)
        }
    }

    private fun startQRCodeScanner() {
        val intent = Intent(context, QRCodeScannerActivity::class.java)
        startActivityForResult(intent, QRCodeScannerActivity.REQUEST_CODE_SCAN)
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        
        if (requestCode == QRCodeScannerActivity.REQUEST_CODE_SCAN && resultCode == Activity.RESULT_OK) {
            val scanResult = data?.getParcelableExtra<QRCodeScannerActivity.QRScanResult>(QRCodeScannerActivity.EXTRA_SCAN_RESULT)
            if (scanResult != null) {
                // 填充扫描结果
                if (scanResult.ip.isNotEmpty()) {
                    binding.ipInput.setText(scanResult.ip)
                }
                if (scanResult.port != 9999) {
                    binding.portInput.setText(scanResult.port.toString())
                }
                if (scanResult.apiKey.isNotEmpty()) {
                    binding.passwordInput.setText(scanResult.apiKey)
                }
                
                // 保存扫描后的连接信息
                saveConnectionInfo(scanResult.ip, scanResult.port, scanResult.apiKey)
                Toast.makeText(context, "扫描成功，连接信息已保存", Toast.LENGTH_SHORT).show()
                
                // 自动连接到扫描的设备
                connectToRemoteDevice()
            }
        }
    }

    private suspend fun fetchRemoteDeviceInfo(ip: String, port: Int, apiKey: String): ApiResult {
        return try {
            // 使用正确的URL格式：http://ip:port/api_key/api/status
            val url = URL("http://$ip:$port/$apiKey/api/status")
            println("RemoteControl: 尝试连接 $url")
            
            val connection = url.openConnection() as HttpURLConnection
            connection.requestMethod = "GET"
            connection.connectTimeout = 10000 // 增加超时时间
            connection.readTimeout = 10000
            connection.setRequestProperty("Accept", "application/json")
            connection.setRequestProperty("Content-Type", "application/json")

            val responseCode = connection.responseCode
            println("RemoteControl: 响应代码 $responseCode")
            
            val response = if (responseCode == 200) {
                connection.inputStream.bufferedReader().use { it.readText() }
            } else {
                connection.errorStream?.bufferedReader()?.use { it.readText() } ?: ""
            }
            
            println("RemoteControl: 响应内容 $response")
            connection.disconnect()

            if (responseCode == 200) {
                val json = JSONObject(response)
                if (json.optBoolean("success", false)) {
                    ApiResult(true, json.getJSONObject("data"), null, null)
                } else {
                    ApiResult(false, null, json.optString("error"), json.optString("message"))
                }
            } else {
                ApiResult(false, null, "HTTP $responseCode", response)
            }
        } catch (e: Exception) {
            println("RemoteControl: 异常 ${e.message}")
            e.printStackTrace()
            ApiResult(false, null, "网络错误", e.message)
        }
    }

    private suspend fun remoteWifiControl(ip: String, port: Int, apiKey: String, start: Boolean): ApiResult {
        return try {
            val endpoint = if (start) "start" else "stop"
            // 使用正确的URL格式：http://ip:port/api_key/api/wifi/start|stop
            val url = URL("http://$ip:$port/$apiKey/api/wifi/$endpoint")
            println("RemoteControl: WiFi控制请求 $url")
            
            val connection = url.openConnection() as HttpURLConnection
            connection.requestMethod = "POST"
            connection.connectTimeout = 10000 // 增加超时时间
            connection.readTimeout = 10000
            connection.setRequestProperty("Accept", "application/json")
            connection.setRequestProperty("Content-Type", "application/json")

            val responseCode = connection.responseCode
            println("RemoteControl: WiFi控制响应代码 $responseCode")
            
            val response = if (responseCode == 200) {
                connection.inputStream.bufferedReader().use { it.readText() }
            } else {
                connection.errorStream?.bufferedReader()?.use { it.readText() } ?: ""
            }
            
            println("RemoteControl: WiFi控制响应内容 $response")
            connection.disconnect()

            if (responseCode == 200) {
                val json = JSONObject(response)
                if (json.optBoolean("success", false)) {
                    ApiResult(true, null, null, json.optString("message"))
                } else {
                    ApiResult(false, null, json.optString("error"), json.optString("message"))
                }
            } else {
                ApiResult(false, null, "HTTP $responseCode", response)
            }
        } catch (e: Exception) {
            println("RemoteControl: WiFi控制异常 ${e.message}")
            e.printStackTrace()
            ApiResult(false, null, "网络错误", e.message)
        }
    }

    override fun onDestroyView() {
        super.onDestroyView()
        _binding = null
    }

    data class ApiResult(
        val success: Boolean,
        val data: JSONObject?,
        val error: String?,
        val message: String?
    )
} 