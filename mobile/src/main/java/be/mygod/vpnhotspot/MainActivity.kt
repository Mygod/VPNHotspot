package be.mygod.vpnhotspot

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.lifecycle.coroutineScope
import be.mygod.vpnhotspot.client.ClientViewModel
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.ui.VpnHotspotApp
import be.mygod.vpnhotspot.ui.theme.VpnHotspotTheme
import kotlinx.coroutines.launch

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        val model by viewModels<ClientViewModel>()
        lifecycle.addObserver(model)
        WifiDoubleLock.ActivityListener(this)
        lifecycle.coroutineScope.launch { BootReceiver.startIfEnabled() }
        setContent {
            VpnHotspotTheme { VpnHotspotApp(model) }
        }
    }
}
