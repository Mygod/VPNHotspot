package be.mygod.vpnhotspot

import android.os.Bundle
import android.view.MenuItem
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.appcompat.app.AppCompatActivity
import androidx.core.graphics.Insets
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.fragment.app.Fragment
import androidx.lifecycle.lifecycleScope
import be.mygod.vpnhotspot.client.ClientViewModel
import be.mygod.vpnhotspot.client.ClientsFragment
import be.mygod.vpnhotspot.databinding.ActivityMainBinding
import be.mygod.vpnhotspot.manage.TetheringFragment
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.google.android.material.navigation.NavigationBarView
import kotlinx.coroutines.launch
import java.net.Inet4Address

class MainActivity : AppCompatActivity(), NavigationBarView.OnItemSelectedListener {
    lateinit var binding: ActivityMainBinding

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        WindowCompat.getInsetsController(window, window.decorView).isAppearanceLightStatusBars = false
        binding = ActivityMainBinding.inflate(layoutInflater)
        setContentView(binding.root)
        ViewCompat.setOnApplyWindowInsetsListener(binding.appBar) { view, insets ->
            val tappable = insets.getInsets(WindowInsetsCompat.Type.tappableElement())
            view.setPadding(tappable.left, tappable.top, tappable.right, 0)
            WindowInsetsCompat.Builder(insets).apply {
                setInsets(WindowInsetsCompat.Type.tappableElement(), Insets.of(0, 0, 0, tappable.bottom))
            }.build()
        }
        binding.navigation.setOnItemSelectedListener(this)
        val badge = binding.navigation.getOrCreateBadge(R.id.navigation_clients).apply {
            backgroundColor = resources.getColor(R.color.colorSecondary, theme)
            badgeTextColor = resources.getColor(androidx.appcompat.R.color.primary_text_default_material_light, theme)
        }
        if (savedInstanceState == null) displayFragment(TetheringFragment())
        val model by viewModels<ClientViewModel>()
        lifecycle.addObserver(model)
        if (Services.p2p != null) ServiceForegroundConnector(this, model, RepeaterService::class)
        model.clients.observe(this) { clients ->
            val count = clients.count {
                it.ip.any { (ip, info) -> ip is Inet4Address && info.state == IpNeighbour.State.VALID }
            }
            badge.isVisible = count > 0
            badge.number = count
        }
        SmartSnackbar.Register(binding.fragmentHolder)
        WifiDoubleLock.ActivityListener(this)
        lifecycleScope.launch { BootReceiver.startIfEnabled() }
    }

    override fun onNavigationItemSelected(item: MenuItem) = when (item.itemId) {
        R.id.navigation_clients -> {
            displayFragment(ClientsFragment())
            true
        }
        R.id.navigation_tethering -> {
            displayFragment(TetheringFragment())
            true
        }
        R.id.navigation_settings -> {
            displayFragment(SettingsPreferenceFragment())
            true
        }
        else -> false
    }

    private fun displayFragment(fragment: Fragment) =
            supportFragmentManager.beginTransaction().replace(R.id.fragmentHolder, fragment).commitAllowingStateLoss()
}
