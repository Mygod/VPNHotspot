package be.mygod.vpnhotspot

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.Toast
import androidx.fragment.app.Fragment
import androidx.lifecycle.lifecycleScope
import be.mygod.vpnhotspot.databinding.FragmentRemoteControlBinding
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import java.net.URLEncoder

class RemoteControlFragment : Fragment() {
    private var _binding: FragmentRemoteControlBinding? = null
    private val binding get() = _binding!!

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
        
        setupUI()
        setupListeners()
    }

    private fun setupUI() {
        // 设置初始状态
        binding.remoteStatusCard.visibility = View.GONE
        binding.progressBar.visibility = View.GONE
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
        binding.temperature.text = "${data.optDouble("temperature", 0.0)}°C"
        binding.cpuUsage.text = "${data.optDouble("cpu", 0.0)}%"
        binding.wifiStatus.text = data.optString("wifiStatus", "未知")
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
                Toast.makeText(context, "扫描成功", Toast.LENGTH_SHORT).show()
            }
        }
    }

    private suspend fun fetchRemoteDeviceInfo(ip: String, port: Int, apiKey: String): ApiResult {
        return try {
            // 使用新的URL格式：http://ip:port/api_key/api/system/info
            val url = URL("http://$ip:$port/$apiKey/api/system/info")
            val connection = url.openConnection() as HttpURLConnection
            connection.requestMethod = "GET"
            connection.connectTimeout = 5000
            connection.readTimeout = 5000

            val responseCode = connection.responseCode
            val response = if (responseCode == 200) {
                connection.inputStream.bufferedReader().use { it.readText() }
            } else {
                connection.errorStream?.bufferedReader()?.use { it.readText() } ?: ""
            }

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
            ApiResult(false, null, "网络错误", e.message)
        }
    }

    private suspend fun remoteWifiControl(ip: String, port: Int, apiKey: String, start: Boolean): ApiResult {
        return try {
            val endpoint = if (start) "start" else "stop"
            // 使用新的URL格式：http://ip:port/api_key/api/wifi/start|stop
            val url = URL("http://$ip:$port/$apiKey/api/wifi/$endpoint")
            val connection = url.openConnection() as HttpURLConnection
            connection.requestMethod = "POST"
            connection.connectTimeout = 5000
            connection.readTimeout = 5000

            val responseCode = connection.responseCode
            val response = if (responseCode == 200) {
                connection.inputStream.bufferedReader().use { it.readText() }
            } else {
                connection.errorStream?.bufferedReader()?.use { it.readText() } ?: ""
            }

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