package be.mygod.vpnhotspot

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.databinding.BaseObservable
import android.databinding.Bindable
import android.databinding.DataBindingUtil
import android.os.Bundle
import android.os.IBinder
import android.support.v4.content.ContextCompat
import android.support.v7.app.AppCompatActivity
import be.mygod.vpnhotspot.databinding.MainActivityBinding

class MainActivity : AppCompatActivity(), ServiceConnection {
    inner class Data : BaseObservable() {
        val switchEnabled: Boolean
            @Bindable get() = when (binder?.status) {
                HotspotService.Status.IDLE -> true
                HotspotService.Status.ACTIVE -> true
                else -> false
            }
        var serviceStarted: Boolean
            @Bindable get() = when (binder?.status) {
                HotspotService.Status.STARTING -> true
                HotspotService.Status.ACTIVE -> true
                else -> false
            }
            set(value) {
                val binder = binder
                when (binder?.status) {
                    HotspotService.Status.IDLE ->
                        ContextCompat.startForegroundService(this@MainActivity,
                                Intent(this@MainActivity, HotspotService::class.java))
                    HotspotService.Status.ACTIVE -> binder.shutdown()
                }
            }

        fun onStatusChanged() {
            notifyPropertyChanged(BR.switchEnabled)
            notifyPropertyChanged(BR.serviceStarted)
        }
        fun onBinderChanged() {
            onStatusChanged()
        }
    }

    private lateinit var binding: MainActivityBinding
    private val data = Data()
    private var binder: HotspotService.HotspotBinder? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        binding = DataBindingUtil.setContentView(this, R.layout.main_activity)
        binding.data = data
    }

    override fun onStart() {
        super.onStart()
        bindService(Intent(this, HotspotService::class.java), this, Context.BIND_AUTO_CREATE)
    }

    override fun onStop() {
        unbindService(this)
        super.onStop()
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        val binder = service as HotspotService.HotspotBinder
        binder.data = data
        this.binder = binder
        data.onBinderChanged()
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        binder?.data = null
        binder = null
    }
}
