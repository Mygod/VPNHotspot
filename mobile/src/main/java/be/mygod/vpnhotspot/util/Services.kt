package be.mygod.vpnhotspot.util

import android.content.Context
import android.net.ConnectivityManager
import android.net.wifi.WifiManager
import android.net.wifi.p2p.WifiP2pManager
import androidx.core.content.getSystemService
import timber.log.Timber

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
            Timber.w(e)
            null
        }
    }
    val wifi by lazy { context.getSystemService<WifiManager>()!! }
}
