package be.mygod.vpnhotspot.util

import android.annotation.SuppressLint
import android.content.Context
import android.net.ConnectivityManager
import android.net.wifi.WifiManager
import android.net.wifi.p2p.WifiP2pManager
import android.util.Log
import androidx.core.content.getSystemService
import timber.log.Timber

@SuppressLint("LogNotTimber")
object Services {
    private lateinit var contextInit: () -> Context
    val context by lazy { contextInit() }
    fun init(context: () -> Context) {
        contextInit = context
    }

    val connectivity by lazy { context.getSystemService<ConnectivityManager>()!! }
    val p2p by lazy {
        try {
            context.getSystemService<WifiP2pManager>()
        } catch (e: RuntimeException) {
            if (android.os.Process.myUid() == 0) Log.w("WifiP2pManager", e) else Timber.w(e)
            null
        }
    }
    val wifi by lazy { context.getSystemService<WifiManager>()!! }
}
