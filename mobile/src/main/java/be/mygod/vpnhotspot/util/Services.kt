package be.mygod.vpnhotspot.util

import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkRequest
import android.net.wifi.WifiManager
import android.net.wifi.p2p.WifiP2pManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import androidx.core.content.getSystemService
import timber.log.Timber

object Services {
    private lateinit var contextInit: () -> Context
    val context by lazy { contextInit() }
    fun init(context: () -> Context) {
        contextInit = context
    }

    val mainHandler by lazy { Handler(Looper.getMainLooper()) }
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

    fun registerNetworkCallbackCompat(request: NetworkRequest, networkCallback: ConnectivityManager.NetworkCallback) =
        if (Build.VERSION.SDK_INT >= 26) connectivity.registerNetworkCallback(request, networkCallback, mainHandler)
        else connectivity.registerNetworkCallback(request, networkCallback)
}
