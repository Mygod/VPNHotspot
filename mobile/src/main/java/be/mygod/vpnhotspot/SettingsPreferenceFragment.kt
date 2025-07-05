package be.mygod.vpnhotspot

import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.os.ext.SdkExtensions
import androidx.core.content.FileProvider
import androidx.lifecycle.lifecycleScope
import androidx.preference.Preference
import androidx.preference.PreferenceFragmentCompat
import androidx.preference.TwoStatePreference
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetherOffloadManager
import be.mygod.vpnhotspot.net.monitor.FallbackUpstreamMonitor
import be.mygod.vpnhotspot.net.monitor.IpMonitor
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.preference.AutoCompleteNetworkPreferenceDialogFragment
import be.mygod.vpnhotspot.preference.SharedPreferenceDataStore
import be.mygod.vpnhotspot.preference.SummaryFallbackProvider
import be.mygod.vpnhotspot.preference.UpstreamsPreference
import be.mygod.vpnhotspot.root.Dump
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.ApiKeyManager
import be.mygod.vpnhotspot.util.WebServerManager
import be.mygod.vpnhotspot.util.QRCodeGenerator
import be.mygod.vpnhotspot.StaticIpSetter
import be.mygod.vpnhotspot.util.launchUrl
import be.mygod.vpnhotspot.util.showAllowingStateLoss
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.google.android.gms.oss.licenses.OssLicensesMenuActivity
import android.view.LayoutInflater
import android.widget.EditText
import android.widget.ImageButton
import android.widget.Toast
import com.google.android.material.snackbar.Snackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.io.PrintWriter
import kotlin.system.exitProcess

class SettingsPreferenceFragment : PreferenceFragmentCompat() {
    private fun Preference.remove() = parent!!.removePreference(this)
    override fun onCreatePreferences(savedInstanceState: Bundle?, rootKey: String?) {
        // handle complicated default value and possible system upgrades
        WifiDoubleLock.mode = WifiDoubleLock.mode
        RoutingManager.masqueradeMode = RoutingManager.masqueradeMode
        IpMonitor.currentMode = IpMonitor.currentMode
        preferenceManager.preferenceDataStore = SharedPreferenceDataStore(app.pref)
        addPreferencesFromResource(R.xml.pref_settings)
        findPreference<UpstreamsPreference>("service.upstream.monitor")!!.attachListener(lifecycle)
        SummaryFallbackProvider(findPreference(UpstreamMonitor.KEY)!!)
        SummaryFallbackProvider(findPreference(FallbackUpstreamMonitor.KEY)!!)
        findPreference<TwoStatePreference>("system.enableTetherOffload")!!.apply {
            isChecked = TetherOffloadManager.enabled
            setOnPreferenceChangeListener { _, newValue ->
                if (TetherOffloadManager.enabled != newValue) viewLifecycleOwner.lifecycleScope.launch {
                    isEnabled = false
                    try {
                        TetherOffloadManager.setEnabled(newValue as Boolean)
                    } catch (_: CancellationException) {
                    } catch (e: Exception) {
                        Timber.w(e)
                        SmartSnackbar.make(e).show()
                    }
                    isChecked = TetherOffloadManager.enabled
                    isEnabled = true
                }
                false
            }
        }
        findPreference<TwoStatePreference>(BootReceiver.KEY)!!.setOnPreferenceChangeListener { _, value ->
            BootReceiver.onUserSettingUpdated(value as Boolean)
            true
        }
        if (Services.p2p == null || !RepeaterService.safeModeConfigurable) {
            val safeMode = findPreference<Preference>(RepeaterService.KEY_SAFE_MODE)!!
            safeMode.remove()
        }
        if (Build.VERSION.SDK_INT < 30) findPreference<Preference>(LocalOnlyHotspotService.KEY_USE_SYSTEM)!!.remove()
        findPreference<Preference>("service.clean")!!.setOnPreferenceClickListener {
            GlobalScope.launch { RoutingManager.clean() }
            true
        }
        findPreference<Preference>(IpMonitor.KEY)!!.setOnPreferenceChangeListener { _, _ ->
            Snackbar.make(requireView(), R.string.settings_restart_required, Snackbar.LENGTH_LONG).apply {
                setAction(R.string.settings_exit_app) {
                    GlobalScope.launch {
                        RoutingManager.clean(false)
                        exitProcess(0)
                    }
                }
            }.show()
            true
        }
        findPreference<Preference>("misc.logcat")!!.setOnPreferenceClickListener {
            GlobalScope.launch(Dispatchers.IO) {
                val context = requireContext()
                val logDir = File(context.cacheDir, "log")
                logDir.mkdir()
                val logFile = File.createTempFile("vpnhotspot-", ".log", logDir)
                logFile.outputStream().use { out ->
                    PrintWriter(out.bufferedWriter()).use { writer ->
                        writer.println("${BuildConfig.VERSION_CODE} is running on API ${Build.VERSION.SDK_INT}")
                        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) writer.println(
                            "S extension ${SdkExtensions.getExtensionVersion(Build.VERSION_CODES.S)}")
                        writer.println()
                        writer.flush()
                        try {
                            Runtime.getRuntime().exec(arrayOf("logcat", "-d")).inputStream.use { it.copyTo(out) }
                        } catch (e: IOException) {
                            Timber.w(e)
                            e.printStackTrace(writer)
                        }
                        writer.println()
                    }
                }
                try {
                    RootManager.use {
                        it.execute(Dump(logFile.absolutePath))
                    }
                } catch (e: Exception) {
                    if (e !is CancellationException) Timber.w(e)
                    PrintWriter(FileOutputStream(logFile, true)).use { e.printStackTrace(it) }
                }
                context.startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND)
                        .setType("text/x-log")
                        .setFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                        .putExtra(Intent.EXTRA_STREAM,
                                FileProvider.getUriForFile(context, "be.mygod.vpnhotspot.log", logFile)),
                        context.getString(androidx.appcompat.R.string.abc_shareactionprovider_share_with)))
            }
            true
        }
        findPreference<Preference>("misc.source")!!.setOnPreferenceClickListener {
            requireContext().launchUrl("https://github.com/Mygod/VPNHotspot/blob/master/README.md")
            true
        }
        findPreference<Preference>("misc.donate")!!.setOnPreferenceClickListener {
            EBegFragment().showAllowingStateLoss(parentFragmentManager, "EBegFragment")
            true
        }
        findPreference<Preference>("misc.licenses")!!.setOnPreferenceClickListener {
            startActivity(Intent(context, OssLicensesMenuActivity::class.java))
            true
        }
        
        // Web服务器设置
        setupWebServerPreferences()
    }

    private fun setupWebServerPreferences() {
        // 开发者调试模式开关
        val developerModePreference = findPreference<TwoStatePreference>("web.server.developer_mode")!!
        developerModePreference.apply {
            isChecked = ApiKeyManager.isDeveloperModeEnabled()
            setOnPreferenceChangeListener { _, newValue ->
                if (newValue as Boolean) {
                    // 显示警告对话框
                    androidx.appcompat.app.AlertDialog.Builder(requireContext())
                        .setTitle("安全警告")
                        .setMessage(R.string.settings_developer_mode_warning)
                        .setPositiveButton("我了解风险，继续启用") { _, _ ->
                            ApiKeyManager.enableDeveloperMode()
                            isChecked = true
                        }
                        .setNegativeButton("取消") { _, _ ->
                            isChecked = false
                        }
                        .setCancelable(false)
                        .show()
                    false // 阻止默认行为，由对话框处理
                } else {
                    ApiKeyManager.disableDeveloperMode()
                    true
                }
            }
        }

        // API Key认证开关
        val apiKeyAuthPreference = findPreference<TwoStatePreference>("web.server.api_key_auth")!!
        apiKeyAuthPreference.apply {
            isChecked = ApiKeyManager.isApiKeyAuthEnabled()
            setOnPreferenceChangeListener { _, newValue ->
                if (newValue as Boolean) {
                    ApiKeyManager.enableApiKeyAuth()
                } else {
                    ApiKeyManager.disableApiKeyAuth()
                }
                true
            }
        }

        // API Key管理
        val apiKeyPreference = findPreference<Preference>("web.server.api_key")!!
        apiKeyPreference.apply {
            summary = if (ApiKeyManager.hasApiKey()) "已设置API Key" else "未设置API Key"
            setOnPreferenceClickListener {
                showApiKeyManagementDialog()
                true
            }
        }

        // 端口设置
        val portPreference = findPreference<Preference>("web.server.port")!!
        portPreference.apply {
            summary = "当前端口: ${WebServerManager.getPort()}"
            setOnPreferenceClickListener {
                showPortInputDialog()
                true
            }
        }
    }

    private fun showApiKeyManagementDialog() {
        val currentApiKey = ApiKeyManager.getApiKey()
        val options = if (currentApiKey != null) {
            arrayOf("生成新API Key", "显示二维码", "复制后台地址", "在浏览器中打开", "重置为默认API Key", "移除API Key")
        } else {
            arrayOf("生成新API Key", "手动输入API Key")
        }

        androidx.appcompat.app.AlertDialog.Builder(requireContext())
            .setTitle("API Key管理")
            .setItems(options) { _, which ->
                when (which) {
                    0 -> generateNewApiKey()
                    1 -> if (currentApiKey != null) {
                        showApiKeyQRCode(currentApiKey)
                    } else {
                        showManualApiKeyInput()
                    }
                    2 -> if (currentApiKey != null) {
                        copyWebBackendUrlToClipboard(currentApiKey)
                    }
                    3 -> if (currentApiKey != null) {
                        openWebBackendInBrowser(currentApiKey)
                    }
                    4 -> if (currentApiKey != null) {
                        resetToDefaultApiKey()
                    }
                    5 -> if (currentApiKey != null) {
                        removeApiKey()
                    }
                }
            }
            .show()
    }

    private fun generateNewApiKey() {
        val newApiKey = ApiKeyManager.generateApiKey()
        ApiKeyManager.setApiKey(newApiKey)
        findPreference<Preference>("web.server.api_key")!!.summary = "已设置API Key"
        Toast.makeText(requireContext(), "新API Key已生成", Toast.LENGTH_SHORT).show()
        showApiKeyQRCode(newApiKey)
    }

    private fun showApiKeyQRCode(apiKey: String) {
        // 获取设备IP地址和端口
        val ip = getDeviceIpAddress()
        val port = WebServerManager.getPort()
        
        if (ip != null) {
            // 生成包含完整URL的二维码
            val qrCodeDialog = QRCodeDialog.newInstance(
                QRCodeGenerator.generateWebAccessQRCode(ip, port, apiKey),
                "Web后台访问二维码",
                "扫描此二维码可直接访问Web后台"
            )
            qrCodeDialog.show(parentFragmentManager, "QRCodeDialog")
        } else {
            // 如果无法获取IP，只显示API Key
            val qrCodeDialog = QRCodeDialog.newInstance(apiKey, "API Key二维码")
            qrCodeDialog.show(parentFragmentManager, "QRCodeDialog")
        }
    }
    
        private fun getDeviceIpAddress(): String? {
        return try {
            // 首先尝试获取用户设置的静态IP地址
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
                        
                        // 优先选择私有IP地址（内网地址）
                        if (ip?.startsWith("192.168.") == true || ip?.startsWith("10.") == true || ip?.startsWith("172.") == true) {
                            return ip
                        }
                    }
                }
            }
            
            // 如果没有找到私有IP，返回第一个有效的IPv4地址
            val networkInterfaces2 = java.net.NetworkInterface.getNetworkInterfaces()
            while (networkInterfaces2.hasMoreElements()) {
                val networkInterface = networkInterfaces2.nextElement()
                if (networkInterface.isLoopback || !networkInterface.isUp) {
                    continue
                }
                
                val inetAddresses = networkInterface.inetAddresses
                while (inetAddresses.hasMoreElements()) {
                    val inetAddress = inetAddresses.nextElement()
                    if (inetAddress is java.net.Inet4Address && !inetAddress.isLoopbackAddress) {
                        return inetAddress.hostAddress
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

    private fun showManualApiKeyInput() {
        val dialogView = LayoutInflater.from(requireContext()).inflate(R.layout.dialog_api_key_input, null)
        val apiKeyInput = dialogView.findViewById<EditText>(R.id.apiKeyInput)

        androidx.appcompat.app.AlertDialog.Builder(requireContext())
            .setTitle("输入API Key")
            .setView(dialogView)
            .setPositiveButton("确定") { _, _ ->
                val apiKey = apiKeyInput.text.toString().trim()
                if (apiKey.isNotEmpty()) {
                    ApiKeyManager.setApiKey(apiKey)
                    findPreference<Preference>("web.server.api_key")!!.summary = "已设置API Key"
                    Toast.makeText(requireContext(), "API Key已设置", Toast.LENGTH_SHORT).show()
                }
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun copyWebBackendUrlToClipboard(apiKey: String) {
        val ip = getDeviceIpAddress()
        val port = WebServerManager.getPort()
        
        if (ip != null) {
            val webBackendUrl = "http://$ip:$port/$apiKey"
            val clipboard = requireContext().getSystemService(android.content.Context.CLIPBOARD_SERVICE) as android.content.ClipboardManager
            val clip = android.content.ClipData.newPlainText("Web后台地址", webBackendUrl)
            clipboard.setPrimaryClip(clip)
            Toast.makeText(requireContext(), "Web后台地址已复制到剪贴板", Toast.LENGTH_SHORT).show()
        } else {
            // 如果无法获取IP地址，回退到复制API Key
            val clipboard = requireContext().getSystemService(android.content.Context.CLIPBOARD_SERVICE) as android.content.ClipboardManager
            val clip = android.content.ClipData.newPlainText("API Key", apiKey)
            clipboard.setPrimaryClip(clip)
            Toast.makeText(requireContext(), "无法获取IP地址，已复制API Key", Toast.LENGTH_SHORT).show()
        }
    }

    private fun openWebBackendInBrowser(apiKey: String) {
        val ip = getDeviceIpAddress()
        val port = WebServerManager.getPort()
        
        if (ip != null) {
            val webBackendUrl = "http://$ip:$port/$apiKey"
            try {
                val intent = android.content.Intent(android.content.Intent.ACTION_VIEW, android.net.Uri.parse(webBackendUrl))
                startActivity(intent)
                Toast.makeText(requireContext(), "正在打开Web后台", Toast.LENGTH_SHORT).show()
            } catch (e: Exception) {
                Toast.makeText(requireContext(), "无法打开浏览器: ${e.message}", Toast.LENGTH_SHORT).show()
            }
        } else {
            Toast.makeText(requireContext(), "无法获取设备IP地址", Toast.LENGTH_SHORT).show()
        }
    }

    private fun resetToDefaultApiKey() {
        androidx.appcompat.app.AlertDialog.Builder(requireContext())
            .setTitle("重置为默认API Key")
            .setMessage("确定要重置为默认API Key吗？\n\n默认API Key: default_api_key_for_debug_2024\n\n认证开关状态保持不变。")
            .setPositiveButton("重置") { _, _ ->
                // 设置默认API Key
                ApiKeyManager.setApiKey("default_api_key_for_debug_2024")
                // 更新UI
                findPreference<Preference>("web.server.api_key")!!.summary = "已设置默认API Key"
                Toast.makeText(requireContext(), "已重置为默认API Key", Toast.LENGTH_SHORT).show()
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun removeApiKey() {
        androidx.appcompat.app.AlertDialog.Builder(requireContext())
            .setTitle("确认移除")
            .setMessage("确定要移除当前的API Key吗？")
            .setPositiveButton("移除") { _, _ ->
                ApiKeyManager.clearApiKey()
                findPreference<Preference>("web.server.api_key")!!.summary = "未设置API Key"
                Toast.makeText(requireContext(), "API Key已移除", Toast.LENGTH_SHORT).show()
            }
            .setNegativeButton("取消", null)
            .show()
    }

    private fun showPortInputDialog() {
        val dialogView = LayoutInflater.from(requireContext()).inflate(R.layout.dialog_port_input, null)
        val portInput = dialogView.findViewById<EditText>(R.id.portInput)
        portInput.setText(WebServerManager.getPort().toString())

        androidx.appcompat.app.AlertDialog.Builder(requireContext())
            .setTitle("设置端口")
            .setView(dialogView)
            .setPositiveButton("确定") { _, _ ->
                val portText = portInput.text.toString().trim()
                val port = portText.toIntOrNull()
                if (port != null && port in 1024..65535) {
                    WebServerManager.setPort(port)
                    findPreference<Preference>("web.server.port")!!.summary = "当前端口: $port"
                    Toast.makeText(requireContext(), "端口已设置为 $port，重启后生效", Toast.LENGTH_SHORT).show()
                } else {
                    Toast.makeText(requireContext(), "请输入有效的端口号 (1024-65535)", Toast.LENGTH_SHORT).show()
                }
            }
            .setNegativeButton("取消", null)
            .show()
    }

    override fun onDisplayPreferenceDialog(preference: Preference) = when (preference.key) {
        UpstreamMonitor.KEY, FallbackUpstreamMonitor.KEY ->
            AutoCompleteNetworkPreferenceDialogFragment().apply {
                setArguments(preference.key)
                setTargetFragment(this@SettingsPreferenceFragment, 0)
            }.showAllowingStateLoss(parentFragmentManager, preference.key)
        else -> super.onDisplayPreferenceDialog(preference)
    }
}
