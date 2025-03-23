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
            iface.startsWith("rndis") || iface.startsWith("usb")
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
        // 启动USB网络共享
        val tetherManager = TetheringManagerCompat.getInstance(context)
        tetherManager.startTethering(TetheringManagerCompat.TETHERING_USB, true, null)
    }
}