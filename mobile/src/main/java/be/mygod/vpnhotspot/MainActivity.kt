package be.mygod.vpnhotspot

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.platform.UriHandler
import androidx.lifecycle.coroutineScope
import be.mygod.vpnhotspot.client.ClientViewModel
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.ui.VpnHotspotApp
import be.mygod.vpnhotspot.ui.theme.VpnHotspotTheme
import be.mygod.vpnhotspot.util.launchUrl
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
            val context = LocalContext.current
            val uriHandler = remember(context) {
                object : UriHandler {
                    override fun openUri(uri: String) = context.launchUrl(uri)
                }
            }
            CompositionLocalProvider(LocalUriHandler provides uriHandler) {
                VpnHotspotTheme { VpnHotspotApp(model) }
            }
        }
    }
}
