package be.mygod.vpnhotspot

import android.content.Intent
import android.os.Bundle
import android.view.Gravity
import android.view.MenuItem
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.databinding.DataBindingUtil
import androidx.fragment.app.Fragment
import androidx.lifecycle.Observer
import androidx.lifecycle.ViewModelProviders
import androidx.lifecycle.get
import be.mygod.vpnhotspot.client.ClientViewModel
import be.mygod.vpnhotspot.client.ClientsFragment
import be.mygod.vpnhotspot.databinding.ActivityMainBinding
import be.mygod.vpnhotspot.manage.TetheringFragment
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.google.android.material.bottomnavigation.BottomNavigationMenuView
import com.google.android.material.bottomnavigation.BottomNavigationView
import q.rorbin.badgeview.QBadgeView

class MainActivity : AppCompatActivity(), BottomNavigationView.OnNavigationItemSelectedListener {
    private lateinit var binding: ActivityMainBinding
    private lateinit var badge: QBadgeView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        binding = DataBindingUtil.setContentView(this, R.layout.activity_main)
        binding.setLifecycleOwner(this)
        binding.navigation.setOnNavigationItemSelectedListener(this)
        if (savedInstanceState == null) displayFragment(TetheringFragment())
        badge = QBadgeView(this)
        badge.bindTarget((binding.navigation.getChildAt(0) as BottomNavigationMenuView).getChildAt(1))
        badge.badgeBackgroundColor = ContextCompat.getColor(this, R.color.colorSecondary)
        badge.badgeTextColor = ContextCompat.getColor(this, R.color.primary_text_default_material_light)
        badge.badgeGravity = Gravity.TOP or Gravity.CENTER_HORIZONTAL
        badge.setGravityOffset(16f, 0f, true)
        val model = ViewModelProviders.of(this).get<ClientViewModel>()
        if (RepeaterService.supported) ServiceForegroundConnector(this, model, RepeaterService::class)
        model.clients.observe(this, Observer { badge.badgeNumber = it.size })
        SmartSnackbar.Register(lifecycle, binding.fragmentHolder)
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
