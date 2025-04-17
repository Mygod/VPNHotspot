package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.Handler
import android.os.Looper
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.TetheringManagerCompat.tetheredIfaces
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber

/**
 * USB网络共享自动启动器
 * 负责自动启动USB网络共享并定期检查其状态
 */
class UsbTetheringAutoStarter private constructor(private val context: Context) {
    companion object {
        private const val CHECK_INTERVAL_MS = 1000L // 检查间隔1秒
        private var instance: UsbTetheringAutoStarter? = null
        const val KEY_AUTO_USB_TETHERING = "service.auto.usbTethering"
        
        fun getInstance(context: Context): UsbTetheringAutoStarter {
            if (instance == null) {
                instance = UsbTetheringAutoStarter(context.applicationContext)
            }
            return instance!!
        }
        
        // 检查是否启用了自动USB网络共享
        fun isEnabled(): Boolean = app.pref.getBoolean(KEY_AUTO_USB_TETHERING, false)
    }
    
    private val handler = Handler(Looper.getMainLooper())
    private var isStarted = false
    
    private val checkRunnable = object : Runnable {
        override fun run() {
            checkAndStartTethering()
            handler.postDelayed(this, CHECK_INTERVAL_MS)
        }
    }
    
    /**
     * 启动USB网络共享自动启动器
     */
    fun start() {
        if (isStarted) return
        
        // 检查是否启用了自动USB网络共享功能
        if (!isEnabled()) {
            Timber.d("Auto USB tethering is disabled")
            return
        }
        
        isStarted = true
        handler.post(checkRunnable)
        Timber.i("UsbTetheringAutoStarter started")
    }
    
    /**
     * 停止USB网络共享自动启动器
     */
    fun stop() {
        if (!isStarted) return
        
        isStarted = false
        handler.removeCallbacks(checkRunnable)
        Timber.i("UsbTetheringAutoStarter stopped")
    }
    
    private fun checkAndStartTethering() {
        // 检查USB网络共享是否已经激活
        val intent = context.registerReceiver(null, IntentFilter(TetheringManagerCompat.ACTION_TETHER_STATE_CHANGED))
        val tetherInterfaces = intent?.tetheredIfaces
        
        // 检查是否有USB网络共享接口
        val usbInterfaces = tetherInterfaces?.filter { iface ->
            iface.contains("rndis") || iface.startsWith("usb")
        } ?: emptyList()
        
        if (usbInterfaces.isNotEmpty()) {
            // USB网络共享已经激活，无需操作
            Timber.v("USB tethering is already active: $usbInterfaces")
            return
        }
        
        // 尝试启动USB网络共享
        Timber.d("Starting USB tethering")
        try {
            startTethering()
            Timber.i("USB tethering start request sent")
        } catch (e: Exception) {
            // 启动失败，记录错误信息
            val errorMsg = e.message ?: "Unknown error"
            Timber.w("Failed to start USB tethering: $errorMsg")
            SmartSnackbar.make(errorMsg).show()
        }
    }
    private fun startTethering() {
        Timber.d("Attempting to start Usb tethering via callback")
        TetheringManagerCompat.startTethering(TetheringManagerCompat.TETHERING_USB, true, object : TetheringManagerCompat.StartTetheringCallback {
            override fun onTetheringStarted() {
                Timber.i("Usb tethering started successfully via callback")
                // 通知UI更新 - 查找并更新所有TetheringTileService.Ethernet实例
                updateTileServices()
                // 确保IP转发已启用
                ensureNetworkConnectivity()
            }

            override fun onTetheringFailed(error: Int?) {
                val errorMsg = if (error != null) {
                    "${TetheringManagerCompat.tetherErrorLookup(error)}"
                } else {
                    "Unknown error"
                }
                Timber.w("Failed to start Usb tethering via callback: $errorMsg")
            }
        })
        Timber.v("Usb tethering start request sent")
    }
    /**
     * 更新所有以太网络共享Tile服务的UI状态
     */
    private fun updateTileServices() {
        try {
            // 使用反射获取所有运行中的TileService实例
            val serviceManager = Class.forName("android.service.quicksettings.TileService")
                .getDeclaredMethod("getSystemService", Context::class.java)
                .invoke(null, context)

            if (serviceManager != null) {
                val method = serviceManager.javaClass.getDeclaredMethod("getActiveTileServices")
                method.isAccessible = true
                val services = method.invoke(serviceManager)?.let { it as? Collection<*> } ?: return

                // 查找并更新所有TetheringTileService$Usb实例
                for (service in services) {
                    val className = service?.javaClass?.name ?: continue
                    // 使用更精确的匹配方式，确保找到正确的TetheringTileService$Usb类
                    if (className.contains("TetheringTileService\$Usb")) {
                        try {
                            // 调用updateTile方法更新UI
                            val updateMethod = service.javaClass.getDeclaredMethod("updateTile")
                            updateMethod.isAccessible = true
                            updateMethod.invoke(service)
                            Timber.d("Updated Usb tethering tile UI: $className")
                        } catch (e: Exception) {
                            // 记录详细的异常信息，帮助调试
                            Timber.w("Failed to update tile UI: ${e.message}")
                        }
                    }
                }
            }
        } catch (e: Exception) {
            // 反射可能会失败，但不应影响主要功能
            Timber.w("Failed to update tile services: ${e.message}")
        }
    }

    /**
     * 确保网络连接配置正确，包括IP转发和防火墙规则
     */
    private fun ensureNetworkConnectivity() {
        try {
            // 检查是否有活跃的接口
            val intent = context.registerReceiver(null, IntentFilter(TetheringManagerCompat.ACTION_TETHER_STATE_CHANGED))
            val tetherInterfaces = intent?.tetheredIfaces
            if (tetherInterfaces.isNullOrEmpty()) {
                Timber.w("No tethered interfaces found after enabling usb tethering")
                return
            }

            // 找到接口
            val usbInterfaces = tetherInterfaces.filter { iface ->
                iface.startsWith("rndis") || iface.startsWith("usb")
            }

            if (usbInterfaces.isEmpty()) {
                Timber.w("No usb tethering interfaces found")
                return
            }

            Timber.d("Found usb tethering interfaces: $usbInterfaces")

            // 使用TetheringService确保IP转发已启用
            val serviceIntent = Intent(context, TetheringService::class.java)
                .putExtra(TetheringService.EXTRA_ADD_INTERFACES, usbInterfaces.toTypedArray<String>())

            // 确保TetheringService能够正确配置网络接口
            val monitorIntent = Intent(context, TetheringService::class.java)
                .putStringArrayListExtra(TetheringService.EXTRA_ADD_INTERFACES_MONITOR, ArrayList(usbInterfaces))

            // 启动服务配置网络接口
            context.startForegroundService(serviceIntent)
            context.startForegroundService(monitorIntent)

            Timber.i("Requested TetheringService to configure routing for interfaces: $usbInterfaces")
        } catch (e: Exception) {
            Timber.e("Failed to ensure network connectivity: ${e.message}")
            SmartSnackbar.make("Failed to configure network: ${e.message}").show()
        }
    }
}