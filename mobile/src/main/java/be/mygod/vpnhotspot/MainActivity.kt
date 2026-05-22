package be.mygod.vpnhotspot

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.repeatOnLifecycle
import be.mygod.vpnhotspot.client.ClientViewModel
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.root.daemon.NeighbourState
import be.mygod.vpnhotspot.ui.VpnHotspotApp
import be.mygod.vpnhotspot.ui.theme.VpnHotspotTheme
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.Services
import kotlinx.coroutines.launch
import java.net.Inet4Address

class MainActivity : ComponentActivity() {
    private var validClientCount by mutableIntStateOf(0)

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        val model by viewModels<ClientViewModel>()
        lifecycle.addObserver(model)
        if (Services.p2p != null) ServiceForegroundConnector(this, model, RepeaterService::class)
        lifecycleScope.launch {
            repeatOnLifecycle(Lifecycle.State.STARTED) {
                model.clients.collect { clients ->
                    validClientCount = clients.count {
                        it.ip.any { (ip, info) ->
                            ip is Inet4Address && info.state == NeighbourState.NEIGHBOUR_STATE_VALID
                        }
                    }
                }
            }
        }
        WifiDoubleLock.ActivityListener(this)
        lifecycleScope.launch { BootReceiver.startIfEnabled() }
        setContent {
            VpnHotspotTheme { VpnHotspotApp(model, validClientCount) }
        }
    }
}
