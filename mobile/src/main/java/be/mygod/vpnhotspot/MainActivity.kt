package be.mygod.vpnhotspot

import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.net.Uri
import android.os.Bundle
import android.os.IBinder
import android.view.Gravity
import android.view.MenuItem
import androidx.appcompat.app.AppCompatActivity
import androidx.browser.customtabs.CustomTabsIntent
import androidx.core.content.ContextCompat
import androidx.databinding.DataBindingUtil
import androidx.fragment.app.Fragment
import be.mygod.vpnhotspot.client.ClientMonitorService
import be.mygod.vpnhotspot.client.ClientsFragment
import be.mygod.vpnhotspot.databinding.ActivityMainBinding
import be.mygod.vpnhotspot.manage.TetheringFragment
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.google.android.material.bottomnavigation.BottomNavigationMenuView
import com.google.android.material.bottomnavigation.BottomNavigationView
import q.rorbin.badgeview.QBadgeView
import timber.log.Timber

class MainActivity : AppCompatActivity(), BottomNavigationView.OnNavigationItemSelectedListener, ServiceConnection {
    companion object {
        var current: MainActivity? = null
    }

    private lateinit var binding: ActivityMainBinding
    private lateinit var badge: QBadgeView
    private var clients: ClientMonitorService.Binder? = null
    private val customTabsIntent by lazy {
        CustomTabsIntent.Builder()
                .setToolbarColor(ContextCompat.getColor(this, R.color.colorPrimary))
                .build()
    }

    fun launchUrl(url: Uri) = try {
        customTabsIntent.launchUrl(this, url)
    } catch (e: ActivityNotFoundException) {
        e.printStackTrace()
        Timber.e(e) //Crashlytics.logException(e)
        SmartSnackbar.make(url.toString()).show()
    } catch (e: SecurityException) {
        e.printStackTrace()
        Timber.e(e) //Crashlytics.logException(e)
        SmartSnackbar.make(url.toString()).show()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        binding = DataBindingUtil.setContentView(this, R.layout.activity_main)
        binding.navigation.setOnNavigationItemSelectedListener(this)
        if (savedInstanceState == null) displayFragment(TetheringFragment())
        badge = QBadgeView(this)
        badge.bindTarget((binding.navigation.getChildAt(0) as BottomNavigationMenuView).getChildAt(1))
        badge.badgeBackgroundColor = ContextCompat.getColor(this, R.color.colorSecondary)
        badge.badgeTextColor = ContextCompat.getColor(this, R.color.primary_text_default_material_light)
        badge.badgeGravity = Gravity.TOP or Gravity.CENTER_HORIZONTAL
        badge.setGravityOffset(16f, 0f, true)
        ServiceForegroundConnector(this, this, ClientMonitorService::class)
        SmartSnackbar.Register(binding.fragmentHolder)
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

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        supportFragmentManager.findFragmentByTag("donationsFragment")?.onActivityResult(requestCode, resultCode, data)
    }
}
