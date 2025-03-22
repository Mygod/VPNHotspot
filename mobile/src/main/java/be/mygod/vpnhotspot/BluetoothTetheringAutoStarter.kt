package be.mygod.vpnhotspot

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.content.Context
import android.content.SharedPreferences
import android.os.Handler
import android.os.Looper
import androidx.core.content.getSystemService
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.manage.BluetoothTethering
import be.mygod.vpnhotspot.net.TetheringManagerCompat
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
            initBluetoothTethering()
            bluetoothTethering
        } ?: return
        
        when (tethering.active) {
            true -> {
                // 已经启动，不需要操作
                Timber.v("Bluetooth tethering is already active")
            }
            false -> {
                // 未启动，尝试启动
                Timber.d("Starting bluetooth tethering")
                startTethering()
            }
            null -> {
                // 状态未知，可能是初始化问题，重新初始化
                Timber.d("Bluetooth tethering state unknown, reinitializing")
                tethering.ensureInit(context)
                if (tethering.active == false) {
                    startTethering()
                }
            }
        }
    }
    
    private fun startTethering() {
        bluetoothTethering?.start(object : TetheringManagerCompat.StartTetheringCallback {
            override fun onTetheringStarted() {
                Timber.i("Bluetooth tethering started successfully")
            }
            
            override fun onTetheringFailed(error: Int?) {
                val errorMsg = if (error != null) {
                    "${TetheringManagerCompat.tetherErrorLookup(error)}"
                } else {
                    "Unknown error"
                }
                Timber.w("Failed to start bluetooth tethering: $errorMsg")
            }
        }, context)
    }
}