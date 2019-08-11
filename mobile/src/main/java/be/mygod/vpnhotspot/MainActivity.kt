package be.mygod.vpnhotspot

import android.content.Intent
import android.os.Bundle
import android.view.MenuItem
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.databinding.DataBindingUtil
import androidx.fragment.app.Fragment
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.get
import androidx.lifecycle.observe
import be.mygod.vpnhotspot.client.ClientViewModel
import be.mygod.vpnhotspot.client.ClientsFragment
import be.mygod.vpnhotspot.databinding.ActivityMainBinding
import be.mygod.vpnhotspot.manage.TetheringFragment
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.google.android.material.bottomnavigation.BottomNavigationView
import java.net.Inet4Address

class MainActivity : AppCompatActivity(), BottomNavigationView.OnNavigationItemSelectedListener {
    private lateinit var binding: ActivityMainBinding
    val provider by lazy { ViewModelProvider(this) }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        binding = DataBindingUtil.setContentView(this, R.layout.activity_main)
        binding.lifecycleOwner = this
        binding.navigation.setOnNavigationItemSelectedListener(this)
        if (savedInstanceState == null) displayFragment(TetheringFragment())
        val model = provider.get<ClientViewModel>()
        if (RepeaterService.supported) ServiceForegroundConnector(this, model, RepeaterService::class)
        model.clients.observe(this) { clients ->
            val count = clients.count {
                it.ip.any { (ip, state) -> ip is Inet4Address && state != IpNeighbour.State.FAILED }
            }
            if (count > 0) binding.navigation.getOrCreateBadge(R.id.navigation_clients).apply {
                backgroundColor = ContextCompat.getColor(this@MainActivity, R.color.colorSecondary)
                badgeTextColor = ContextCompat.getColor(this@MainActivity, R.color.primary_text_default_material_light)
                number = count
            } else binding.navigation.removeBadge(R.id.navigation_clients)
        }
        SmartSnackbar.Register(lifecycle, binding.fragmentHolder)
        WifiDoubleLock.ActivityListener(this)
    }

    override fun onNavigationItemSelected(item: MenuItem) = when (item.itemId) {
        R.id.navigation_clients -> {
            if (!item.isChecked) {
                item.isChecked = true
                displayFragment(ClientsFragment())
            }
            true
        }
        R.id.navigation_tethering -> {
            if (!item.isChecked) {
                item.isChecked = true
                displayFragment(TetheringFragment())
            }
            true
        }
        R.id.navigation_settings -> {
            if (!item.isChecked) {
                item.isChecked = true
                displayFragment(SettingsPreferenceFragment())
            }
            true
        }
        else -> false
    }

    private fun displayFragment(fragment: Fragment) =
            supportFragmentManager.beginTransaction().replace(R.id.fragmentHolder, fragment).commitAllowingStateLoss()

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        supportFragmentManager.findFragmentByTag("donationsFragment")?.onActivityResult(requestCode, resultCode, data)
    }
}
