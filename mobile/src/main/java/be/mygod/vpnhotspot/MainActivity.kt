package be.mygod.vpnhotspot

import android.content.ComponentName
import android.content.ServiceConnection
import android.databinding.DataBindingUtil
import android.os.Bundle
import android.os.IBinder
import android.support.design.internal.BottomNavigationMenuView
import android.support.design.widget.BottomNavigationView
import android.support.v4.app.Fragment
import android.support.v4.content.ContextCompat
import android.support.v7.app.AppCompatActivity
import android.view.Gravity
import android.view.MenuItem
import be.mygod.vpnhotspot.client.ClientMonitorService
import be.mygod.vpnhotspot.client.ClientsFragment
import be.mygod.vpnhotspot.databinding.ActivityMainBinding
import be.mygod.vpnhotspot.manage.TetheringFragment
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import q.rorbin.badgeview.QBadgeView

class MainActivity : AppCompatActivity(), BottomNavigationView.OnNavigationItemSelectedListener, ServiceConnection {
    private lateinit var binding: ActivityMainBinding
    private lateinit var badge: QBadgeView
    private var clients: ClientMonitorService.Binder? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        binding = DataBindingUtil.setContentView(this, R.layout.activity_main)
        binding.navigation.setOnNavigationItemSelectedListener(this)
        if (savedInstanceState == null) displayFragment(TetheringFragment())
        badge = QBadgeView(this)
        badge.bindTarget((binding.navigation.getChildAt(0) as BottomNavigationMenuView).getChildAt(1))
        badge.badgeBackgroundColor = ContextCompat.getColor(this, R.color.colorAccent)
        badge.badgeTextColor = ContextCompat.getColor(this, R.color.primary_text_default_material_light)
        badge.badgeGravity = Gravity.TOP or Gravity.CENTER_HORIZONTAL
        badge.setGravityOffset(16f, 0f, true)
        ServiceForegroundConnector(this, this, ClientMonitorService::class)
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

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        clients = service as ClientMonitorService.Binder
        service.clientsChanged[this] = { badge.badgeNumber = it.size }
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        val clients = clients ?: return
        this.clients = null
        clients.clientsChanged -= this
    }

    private fun displayFragment(fragment: Fragment) =
            supportFragmentManager.beginTransaction().replace(R.id.fragmentHolder, fragment).commitAllowingStateLoss()
}
