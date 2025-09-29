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
import android.net.TetheringManager
import be.mygod.vpnhotspot.net.TetherOffloadManager
import be.mygod.vpnhotspot.net.TetheringManagerCompat
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
import android.content.Context
import be.mygod.vpnhotspot.widget.SmartSnackbar
import android.view.LayoutInflater
import android.widget.EditText
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
            requireContext().launchUrl("https://mygod.be/donate/")
            true
        }
        findPreference<Preference>("misc.licenses")!!.setOnPreferenceClickListener {
            startActivity(Intent(requireContext(), AboutLibrariesActivity::class.java))
            true
        }
        
        // 远程控制自动连接开关
        findPreference<TwoStatePreference>("remote.control.auto.connect")!!.setOnPreferenceChangeListener { _, newValue ->
            val newAutoConnect = newValue as Boolean
            
            // 显示提示信息
            val message = if (newAutoConnect) "已启用远程控制自动连接" else "已禁用远程控制自动连接"
            Toast.makeText(requireContext(), message, Toast.LENGTH_SHORT).show()
            
            // 记录设置变化
            Timber.d("Settings: 远程控制自动连接已设置为 $newAutoConnect")
            
            true
        }
        
        // Web服务器设置
        setupWebServerPreferences()
        
        // 自动启动功能设置
        setupAutoStartPreferences()
    }
    
    private fun setupAutoStartPreferences() {
        // 蓝牙网络共享自动启动
        findPreference<TwoStatePreference>("service.auto.bluetoothTethering")!!.setOnPreferenceChangeListener { _, newValue ->
            val enabled = newValue as Boolean
            handleAutoStartChange("蓝牙网络共享", enabled) {
                if (enabled) {
                    BluetoothTetheringAutoStarter.getInstance(requireContext()).start()
                    // 立即启动蓝牙网络共享
                    startBluetoothTetheringImmediately()
                } else {
                    BluetoothTetheringAutoStarter.getInstance(requireContext()).stop()
                }
            }
            true
        }
        
        // WiFi热点自动启动
        findPreference<TwoStatePreference>("service.auto.wifiTethering")!!.setOnPreferenceChangeListener { _, newValue ->
            val enabled = newValue as Boolean
            handleAutoStartChange("WiFi热点", enabled) {
                if (enabled) {
                    WifiTetheringAutoStarter.getInstance(requireContext()).start()
                    // 立即启动WiFi热点
                    startWifiTetheringImmediately()
                } else {
                    WifiTetheringAutoStarter.getInstance(requireContext()).stop()
                }
            }
            true
        }
        
        // 以太网络共享自动启动
        findPreference<TwoStatePreference>("service.auto.ethernetTethering")!!.setOnPreferenceChangeListener { _, newValue ->
            val enabled = newValue as Boolean
            handleAutoStartChange("以太网络共享", enabled) {
                if (enabled) {
                    EthernetTetheringAutoStarter.getInstance(requireContext()).start()
                    // 立即启动以太网络共享
                    startEthernetTetheringImmediately()
                } else {
                    EthernetTetheringAutoStarter.getInstance(requireContext()).stop()
                }
            }
            true
        }
        
        // USB网络共享自动启动
        findPreference<TwoStatePreference>("service.auto.usbTethering")!!.setOnPreferenceChangeListener { _, newValue ->
            val enabled = newValue as Boolean
            handleAutoStartChange("USB网络共享", enabled) {
                if (enabled) {
                    UsbTetheringAutoStarter.getInstance(requireContext()).start()
                    // 立即启动USB网络共享
                    startUsbTetheringImmediately()
                } else {
                    UsbTetheringAutoStarter.getInstance(requireContext()).stop()
                }
            }
            true
        }
    }
    
    private fun handleAutoStartChange(featureName: String, enabled: Boolean, action: () -> Unit) {
        val message = if (enabled) "已启用${featureName}自动启动" else "已禁用${featureName}自动启动"
        Toast.makeText(requireContext(), message, Toast.LENGTH_SHORT).show()
        Timber.d("Settings: ${featureName} 自动启动已设置为 $enabled")
        
        // 执行相应的启动或停止操作
        try {
            action()
        } catch (e: Exception) {
            Timber.w(e, "Failed to handle auto start change for $featureName")
            SmartSnackbar.make("操作失败: ${e.message}").show()
        }
    }
    
    private fun startBluetoothTetheringImmediately() {
        try {
            TetheringManagerCompat.startTethering(TetheringManagerCompat.TETHERING_BLUETOOTH, true, object : TetheringManagerCompat.StartTetheringCallback {
                override fun onTetheringStarted() {
                    Timber.i("Bluetooth tethering started immediately from settings")
                    SmartSnackbar.make("蓝牙网络共享已启动").show()
                }
                
                override fun onTetheringFailed(error: Int?) {
                    val errorMsg = if (error != null) {
                        "启动失败: ${TetheringManagerCompat.tetherErrorLookup(error)}"
                    } else {
                        "启动失败: 未知错误"
                    }
                    Timber.w("Failed to start bluetooth tethering immediately: $errorMsg")
                    SmartSnackbar.make(errorMsg).show()
                }
            })
        } catch (e: Exception) {
            Timber.w(e, "Exception when starting bluetooth tethering immediately")
            SmartSnackbar.make("启动失败: ${e.message}").show()
        }
    }
    
    private fun startWifiTetheringImmediately() {
        try {
            TetheringManagerCompat.startTethering(TetheringManager.TETHERING_WIFI, true, object : TetheringManagerCompat.StartTetheringCallback {
                override fun onTetheringStarted() {
                    Timber.i("WiFi tethering started immediately from settings")
                    SmartSnackbar.make("WiFi热点已启动").show()
                }
                
                override fun onTetheringFailed(error: Int?) {
                    val errorMsg = if (error != null) {
                        "启动失败: ${TetheringManagerCompat.tetherErrorLookup(error)}"
                    } else {
                        "启动失败: 未知错误"
                    }
                    Timber.w("Failed to start WiFi tethering immediately: $errorMsg")
                    SmartSnackbar.make(errorMsg).show()
                }
            })
        } catch (e: Exception) {
            Timber.w(e, "Exception when starting WiFi tethering immediately")
            SmartSnackbar.make("启动失败: ${e.message}").show()
        }
    }
    
    private fun startEthernetTetheringImmediately() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) {
            SmartSnackbar.make("以太网络共享需要Android 11或更高版本").show()
            return
        }
        
        try {
            TetheringManagerCompat.startTethering(TetheringManagerCompat.TETHERING_ETHERNET, true, object : TetheringManagerCompat.StartTetheringCallback {
                override fun onTetheringStarted() {
                    Timber.i("Ethernet tethering started immediately from settings")
                    SmartSnackbar.make("以太网络共享已启动").show()
                }
                
                override fun onTetheringFailed(error: Int?) {
                    val errorMsg = if (error != null) {
                        "启动失败: ${TetheringManagerCompat.tetherErrorLookup(error)}"
                    } else {
                        "启动失败: 未知错误"
                    }
                    Timber.w("Failed to start ethernet tethering immediately: $errorMsg")
                    SmartSnackbar.make(errorMsg).show()
                }
            })
        } catch (e: Exception) {
            Timber.w(e, "Exception when starting ethernet tethering immediately")
            SmartSnackbar.make("启动失败: ${e.message}").show()
        }
    }
    
    private fun startUsbTetheringImmediately() {
        try {
            TetheringManagerCompat.startTethering(TetheringManagerCompat.TETHERING_USB, true, object : TetheringManagerCompat.StartTetheringCallback {
                override fun onTetheringStarted() {
                    Timber.i("USB tethering started immediately from settings")
                    SmartSnackbar.make("USB网络共享已启动").show()
                }
                
                override fun onTetheringFailed(error: Int?) {
                    val errorMsg = if (error != null) {
                        "启动失败: ${TetheringManagerCompat.tetherErrorLookup(error)}"
                    } else {
                        "启动失败: 未知错误"
                    }
                    Timber.w("Failed to start USB tethering immediately: $errorMsg")
                    SmartSnackbar.make(errorMsg).show()
                }
            })
        } catch (e: Exception) {
            Timber.w(e, "Exception when starting USB tethering immediately")
            SmartSnackbar.make("启动失败: ${e.message}").show()
        }
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
            try {
                val staticIps = StaticIpSetter.ips
                if (staticIps.isNotEmpty()) {
                    Timber.d("Checking static IP settings: $staticIps")
                    // 解析静态IP设置，可能包含多个IP（每行一个）
                    val ipLines = staticIps.lines().filter { it.isNotEmpty() }
                    for (ipLine in ipLines) {
                        val ip = ipLine.trim()
                        // 检查是否是有效的IPv4地址
                        if (isValidIPv4(ip)) {
                            Timber.d("Using static IP address: $ip")
                            return ip
                        } else {
                            Timber.w("Invalid static IP address format: $ip")
                        }
                    }
                    Timber.w("No valid static IP addresses found in settings")
                }
            } catch (e: Exception) {
                Timber.w(e, "Failed to read static IP settings, falling back to network interfaces")
            }
            
            // 如果没有设置静态IP或静态IP无效，则获取网络接口的IP
            try {
                val networkInterfaces = java.net.NetworkInterface.getNetworkInterfaces()
                if (networkInterfaces == null) {
                    Timber.w("Network interfaces enumeration returned null")
                    return null
                }
                
                val foundIPs = mutableListOf<String>()
                while (networkInterfaces.hasMoreElements()) {
                    val networkInterface = networkInterfaces.nextElement()
                    
                    // 跳过回环接口和未启用的接口
                    if (networkInterface.isLoopback || !networkInterface.isUp) {
                        continue
                    }
                    
                    Timber.d("Checking network interface: ${networkInterface.name}")
                    
                    // 获取接口的IP地址
                    val inetAddresses = networkInterface.inetAddresses
                    while (inetAddresses.hasMoreElements()) {
                        val inetAddress = inetAddresses.nextElement()
                        
                        // 只选择IPv4地址，排除回环地址
                        if (inetAddress is java.net.Inet4Address && !inetAddress.isLoopbackAddress) {
                            val ip = inetAddress.hostAddress
                            
                            // 返回任何有效的IPv4地址（不限于私有IP）
                            if (ip != null && isValidIPv4(ip)) {
                                foundIPs.add(ip)
                                Timber.d("Found valid IP address: $ip on interface ${networkInterface.name}")
                                return ip
                            }
                        }
                    }
                }
                
                if (foundIPs.isEmpty()) {
                    Timber.w("No valid IPv4 addresses found on any network interface")
                } else {
                    Timber.d("Found IP addresses: $foundIPs")
                }
            } catch (e: SecurityException) {
                Timber.w(e, "Security exception when accessing network interfaces")
            } catch (e: Exception) {
                Timber.w(e, "Exception when enumerating network interfaces")
            }
            
            null
        } catch (e: Exception) {
            Timber.e(e, "Unexpected exception when getting device IP address")
            null
        }
    }
    
    private fun isValidIPv4(ip: String): Boolean {
        return try {
            if (ip.isBlank()) {
                Timber.d("IP validation failed: empty or blank string")
                return false
            }
            
            val parts = ip.split(".")
            if (parts.size != 4) {
                Timber.d("IP validation failed: incorrect number of parts (${parts.size}) in $ip")
                return false
            }
            
            for ((index, part) in parts.withIndex()) {
                if (part.isEmpty()) {
                    Timber.d("IP validation failed: empty part at index $index in $ip")
                    return false
                }
                
                try {
                    val num = part.toInt()
                    if (num < 0 || num > 255) {
                        Timber.d("IP validation failed: part $part (value: $num) out of range [0-255] in $ip")
                        return false
                    }
                } catch (e: NumberFormatException) {
                    Timber.d("IP validation failed: non-numeric part '$part' at index $index in $ip")
                    return false
                }
            }
            
            Timber.d("IP validation successful: $ip")
            true
        } catch (e: Exception) {
            Timber.w(e, "Unexpected exception during IP validation for: $ip")
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
        try {
            val context = requireContext()
            val clipboard = context.getSystemService(android.content.Context.CLIPBOARD_SERVICE) as? android.content.ClipboardManager
            
            if (clipboard == null) {
                Timber.w("Clipboard service is not available")
                Toast.makeText(context, "剪贴板服务不可用", Toast.LENGTH_SHORT).show()
                return
            }
            
            val ip = getDeviceIpAddress()
            val port = WebServerManager.getPort()
            
            if (ip != null) {
                try {
                    val webBackendUrl = "http://$ip:$port/$apiKey"
                    val clip = android.content.ClipData.newPlainText("Web后台地址", webBackendUrl)
                    clipboard.setPrimaryClip(clip)
                    
                    // 验证剪贴板内容是否正确设置
                    val primaryClip = clipboard.primaryClip
                    if (primaryClip != null && primaryClip.itemCount > 0) {
                        val clipText = primaryClip.getItemAt(0).text?.toString()
                        if (clipText == webBackendUrl) {
                            Toast.makeText(context, "Web后台地址已复制到剪贴板", Toast.LENGTH_SHORT).show()
                            Timber.d("Successfully copied web backend URL to clipboard: $webBackendUrl")
                        } else {
                            Timber.w("Clipboard content verification failed. Expected: $webBackendUrl, Got: $clipText")
                            Toast.makeText(context, "剪贴板复制可能不完整，请重试", Toast.LENGTH_SHORT).show()
                        }
                    } else {
                        Timber.w("Failed to verify clipboard content after copy operation")
                        Toast.makeText(context, "剪贴板复制验证失败，请重试", Toast.LENGTH_SHORT).show()
                    }
                } catch (e: SecurityException) {
                    Timber.w(e, "Security exception when copying web backend URL to clipboard")
                    // 回退到复制API Key
                    fallbackCopyApiKey(clipboard, apiKey, context)
                } catch (e: Exception) {
                    Timber.w(e, "Exception when copying web backend URL to clipboard")
                    // 回退到复制API Key
                    fallbackCopyApiKey(clipboard, apiKey, context)
                }
            } else {
                // 如果无法获取IP地址，回退到复制API Key
                Timber.w("Unable to get device IP address, falling back to API Key copy")
                fallbackCopyApiKey(clipboard, apiKey, context)
            }
        } catch (e: SecurityException) {
            Timber.w(e, "Security exception when accessing clipboard service")
            Toast.makeText(requireContext(), "无法访问剪贴板：权限被拒绝", Toast.LENGTH_SHORT).show()
        } catch (e: IllegalStateException) {
            Timber.w(e, "Fragment not attached when accessing clipboard")
            // Fragment可能已经被销毁，无法显示Toast
            Timber.w("Cannot show clipboard error message: Fragment not attached")
        } catch (e: Exception) {
            Timber.w(e, "Unexpected exception when copying to clipboard")
            try {
                Toast.makeText(requireContext(), "剪贴板操作失败：${e.message}", Toast.LENGTH_SHORT).show()
            } catch (fragmentException: Exception) {
                Timber.w(fragmentException, "Cannot show error toast: Fragment issue")
            }
        }
    }
    
    private fun fallbackCopyApiKey(clipboard: android.content.ClipboardManager, apiKey: String, context: Context) {
        try {
            val clip = android.content.ClipData.newPlainText("API Key", apiKey)
            clipboard.setPrimaryClip(clip)
            
            // 验证API Key是否正确复制
            val primaryClip = clipboard.primaryClip
            if (primaryClip != null && primaryClip.itemCount > 0) {
                val clipText = primaryClip.getItemAt(0).text?.toString()
                if (clipText == apiKey) {
                    Toast.makeText(context, "无法获取IP地址，已复制API Key到剪贴板", Toast.LENGTH_SHORT).show()
                    Timber.d("Successfully copied API Key to clipboard as fallback")
                } else {
                    Timber.w("API Key clipboard verification failed. Expected: $apiKey, Got: $clipText")
                    Toast.makeText(context, "API Key复制可能不完整，请重试", Toast.LENGTH_SHORT).show()
                }
            } else {
                Timber.w("Failed to verify API Key clipboard content after copy operation")
                Toast.makeText(context, "API Key复制验证失败，请重试", Toast.LENGTH_SHORT).show()
            }
        } catch (e: SecurityException) {
            Timber.w(e, "Security exception when copying API Key to clipboard as fallback")
            Toast.makeText(context, "无法访问剪贴板：权限被拒绝", Toast.LENGTH_SHORT).show()
        } catch (e: Exception) {
            Timber.w(e, "Exception when copying API Key to clipboard as fallback")
            Toast.makeText(context, "API Key复制失败：${e.message}", Toast.LENGTH_SHORT).show()
        }
    }

    private fun openWebBackendInBrowser(apiKey: String) {
        try {
            val ip = getDeviceIpAddress()
            val port = WebServerManager.getPort()
            
            if (ip != null) {
                val webBackendUrl = "http://$ip:$port/$apiKey"
                Timber.d("Attempting to open web backend URL in browser: $webBackendUrl")
                
                try {
                    val intent = android.content.Intent(android.content.Intent.ACTION_VIEW, android.net.Uri.parse(webBackendUrl))
                    intent.addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK)
                    startActivity(intent)
                    Toast.makeText(requireContext(), "正在打开Web后台", Toast.LENGTH_SHORT).show()
                } catch (e: android.content.ActivityNotFoundException) {
                    Timber.w(e, "No browser app found to handle the URL")
                    Toast.makeText(requireContext(), "未找到可用的浏览器应用", Toast.LENGTH_SHORT).show()
                } catch (e: SecurityException) {
                    Timber.w(e, "Security exception when opening browser")
                    Toast.makeText(requireContext(), "无法打开浏览器：权限被拒绝", Toast.LENGTH_SHORT).show()
                } catch (e: Exception) {
                    Timber.w(e, "Exception when opening browser")
                    Toast.makeText(requireContext(), "无法打开浏览器: ${e.message}", Toast.LENGTH_SHORT).show()
                }
            } else {
                Timber.w("Cannot open web backend in browser: device IP address is null")
                Toast.makeText(requireContext(), "无法获取设备IP地址，无法打开Web后台", Toast.LENGTH_SHORT).show()
            }
        } catch (e: IllegalStateException) {
            Timber.w(e, "Fragment not attached when opening web backend in browser")
            // Fragment可能已经被销毁，无法显示Toast
        } catch (e: Exception) {
            Timber.w(e, "Unexpected exception when opening web backend in browser")
            try {
                Toast.makeText(requireContext(), "打开Web后台失败：${e.message}", Toast.LENGTH_SHORT).show()
            } catch (fragmentException: Exception) {
                Timber.w(fragmentException, "Cannot show error toast: Fragment issue")
            }
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
