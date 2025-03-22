package be.mygod.vpnhotspot

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.SharedPreferences
import android.os.Handler
import android.os.Looper
import androidx.core.content.getSystemService
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.manage.BluetoothTethering
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.TetheringManagerCompat.tetheredIfaces
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber

/**
 * 蓝牙网络共享自动启动器
 * 负责自动启动蓝牙网络共享并定期检查其状态
 */
class BluetoothTetheringAutoStarter private constructor(private val context: Context) {
    companion object {
        private const val CHECK_INTERVAL_MS = 1000L // 检查间隔1秒
        private var instance: BluetoothTetheringAutoStarter? = null
        const val KEY_AUTO_BLUETOOTH_TETHERING = "service.auto.bluetoothTethering"
        
        fun getInstance(context: Context): BluetoothTetheringAutoStarter {
            if (instance == null) {
                instance = BluetoothTetheringAutoStarter(context.applicationContext)
            }
            return instance!!
        }
        
        // 检查是否启用了自动蓝牙网络共享
        fun isEnabled(): Boolean = app.pref.getBoolean(KEY_AUTO_BLUETOOTH_TETHERING, false)
    }
    
    private val handler = Handler(Looper.getMainLooper())
    private var bluetoothTethering: BluetoothTethering? = null
    private var isStarted = false
    
    private val checkRunnable = object : Runnable {
        override fun run() {
            checkAndStartTethering()
            handler.postDelayed(this, CHECK_INTERVAL_MS)
        }
    }
    
    /**
     * 启动蓝牙网络共享自动启动器
     */
    fun start() {
        if (isStarted) return
        
        // 检查是否启用了自动蓝牙网络共享功能
        if (!isEnabled()) {
            Timber.d("Auto bluetooth tethering is disabled")
            return
        }
        
        isStarted = true
        initBluetoothTethering()
        handler.post(checkRunnable)
        Timber.i("BluetoothTetheringAutoStarter started")
    }
    
    /**
     * 停止蓝牙网络共享自动启动器
     */
    fun stop() {
        if (!isStarted) return
        
        isStarted = false
        handler.removeCallbacks(checkRunnable)
        bluetoothTethering?.close()
        bluetoothTethering = null
        Timber.i("BluetoothTetheringAutoStarter stopped")
    }
    
    private fun initBluetoothTethering() {
        if (bluetoothTethering != null) return
        
        val bluetoothManager = context.getSystemService<BluetoothManager>()
        val adapter = bluetoothManager?.adapter
        
        if (adapter == null) {
            Timber.w("BluetoothAdapter not available")
            return
        }
        
        bluetoothTethering = BluetoothTethering(context, adapter) {
            // 状态变化回调
            Timber.d("Bluetooth tethering state changed: ${bluetoothTethering?.active}")
        }
        bluetoothTethering?.ensureInit(context)
    }
    
    private fun checkAndStartTethering() {
        val tethering = bluetoothTethering ?: run {
            Timber.d("Bluetooth tethering not initialized, initializing...")
            initBluetoothTethering()
            bluetoothTethering
        } ?: run {
            Timber.w("Failed to initialize bluetooth tethering")
            return
        }
        
        if (tethering.active == true) {
            // 蓝牙网络共享已经激活，无需操作
            Timber.v("Bluetooth tethering is already active")
            return
        }
        
        // 尝试启动蓝牙网络共享
        Timber.d("Starting bluetooth tethering")
        try {
            startTethering()
            Timber.i("Bluetooth tethering started successfully")
        } catch (e: Exception) {
            // 启动失败，记录错误信息
            val errorMsg = e.message ?: "Unknown error"
            Timber.w("Failed to start bluetooth tethering: $errorMsg")
            SmartSnackbar.make(errorMsg).show()
        }
    }
    
    private fun startTethering() {
        Timber.d("Attempting to start bluetooth tethering via callback")
        bluetoothTethering?.start(object : TetheringManagerCompat.StartTetheringCallback {
            override fun onTetheringStarted() {
                Timber.i("Bluetooth tethering started successfully via callback")
                // 通知UI更新 - 查找并更新所有TetheringTileService.Bluetooth实例
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
                Timber.w("Failed to start bluetooth tethering via callback: $errorMsg")
            }
        }, context)
        Timber.v("Bluetooth tethering start request sent")
    }
    
    /**
     * 更新所有蓝牙网络共享Tile服务的UI状态
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
                
                // 查找并更新所有TetheringTileService.Bluetooth实例
                for (service in services) {
                    val className = service?.javaClass?.name ?: continue
                    // 使用更精确的匹配方式，确保找到正确的TetheringTileService$Bluetooth类
                    if (className.contains("TetheringTileService\$Bluetooth")) {
                        try {
                            // 调用updateTile方法更新UI
                            val updateMethod = service.javaClass.getDeclaredMethod("updateTile")
                            updateMethod.isAccessible = true
                            updateMethod.invoke(service)
                            Timber.d("Updated Bluetooth tethering tile UI: $className")
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
            // 检查是否有活跃的蓝牙网络接口
            val intent = context.registerReceiver(null, IntentFilter(TetheringManagerCompat.ACTION_TETHER_STATE_CHANGED))
            val tetherInterfaces = intent?.tetheredIfaces
            if (tetherInterfaces.isNullOrEmpty()) {
                Timber.w("No tethered interfaces found after enabling bluetooth tethering")
                return
            }
            
            // 找到蓝牙网络接口
            val bluetoothInterfaces = tetherInterfaces.filter { iface ->
                iface.startsWith("bt-pan") || iface.startsWith("bnep")
            }
            
            if (bluetoothInterfaces.isEmpty()) {
                Timber.w("No bluetooth tethering interfaces found")
                return
            }
            
            Timber.d("Found bluetooth tethering interfaces: $bluetoothInterfaces")
            
            // 使用RoutingManager确保IP转发已启用
            // 注意：这里我们不直接调用RoutingManager，因为它需要root权限
            // 而是通过TetheringService来处理，它会自动配置正确的路由
            val serviceIntent = Intent(context, TetheringService::class.java)
                .putExtra(TetheringService.EXTRA_ADD_INTERFACES, bluetoothInterfaces.toTypedArray<String>())
            
            // 确保TetheringService能够正确配置网络接口
            // 使用EXTRA_ADD_INTERFACES_MONITOR参数可以让服务持续监控接口状态
            val monitorIntent = Intent(context, TetheringService::class.java)
                .putStringArrayListExtra(TetheringService.EXTRA_ADD_INTERFACES_MONITOR, ArrayList(bluetoothInterfaces))
                
            // 启动服务配置网络接口
            context.startForegroundService(serviceIntent)
            context.startForegroundService(monitorIntent)
            
            Timber.i("Requested TetheringService to configure routing for interfaces: $bluetoothInterfaces")
            Timber.d("Also requested monitoring for these interfaces to ensure connectivity")
        } catch (e: Exception) {
            Timber.e("Failed to ensure network connectivity: ${e.message}")
            SmartSnackbar.make("Failed to configure network: ${e.message}").show()
        }
    }
}