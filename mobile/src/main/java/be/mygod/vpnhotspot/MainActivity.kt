package be.mygod.vpnhotspot

import android.os.Bundle
import android.view.MenuItem
import androidx.activity.viewModels
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.core.graphics.Insets
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.fragment.app.Fragment
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.repeatOnLifecycle
import be.mygod.vpnhotspot.client.ClientViewModel
import be.mygod.vpnhotspot.client.ClientsFragment
import be.mygod.vpnhotspot.databinding.ActivityMainBinding
import be.mygod.vpnhotspot.manage.TetheringFragment
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.util.AppUpdate
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UpdateChecker
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.google.android.material.badge.BadgeDrawable
import com.google.android.material.navigation.NavigationBarView
import kotlinx.coroutines.launch
import timber.log.Timber
import java.net.Inet4Address
import java.util.concurrent.CancellationException

class MainActivity : AppCompatActivity(), NavigationBarView.OnItemSelectedListener {
    lateinit var binding: ActivityMainBinding
    private lateinit var updateItem: MenuItem
    private lateinit var updateBadge: BadgeDrawable

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        WindowCompat.setDecorFitsSystemWindows(window, false)
        ViewCompat.setOnApplyWindowInsetsListener(findViewById(android.R.id.content)) { view, insets ->
            val statusBarInsets = insets.getInsets(WindowInsetsCompat.Type.statusBars())
            view.setPadding(statusBarInsets.left, statusBarInsets.top, statusBarInsets.right, statusBarInsets.bottom)
            WindowInsetsCompat.Builder(insets).apply {
                setInsets(WindowInsetsCompat.Type.statusBars(), Insets.NONE)
            }.build()
        }
        binding = ActivityMainBinding.inflate(layoutInflater)
        setContentView(binding.root)
        binding.navigation.setOnItemSelectedListener(this)
        val badge = binding.navigation.getOrCreateBadge(R.id.navigation_clients).apply {
            backgroundColor = ContextCompat.getColor(this@MainActivity, R.color.colorSecondary)
            badgeTextColor = ContextCompat.getColor(this@MainActivity,
                androidx.appcompat.R.color.primary_text_default_material_light)
        }
        updateItem = binding.navigation.menu.findItem(R.id.navigation_update)
        updateItem.isCheckable = false
        updateBadge = binding.navigation.getOrCreateBadge(R.id.navigation_update).apply {
            backgroundColor = ContextCompat.getColor(this@MainActivity, R.color.colorSecondary)
            badgeTextColor = ContextCompat.getColor(this@MainActivity,
                androidx.appcompat.R.color.primary_text_default_material_light)
        }
        if (savedInstanceState == null) displayFragment(TetheringFragment())
        val model by viewModels<ClientViewModel>()
        lifecycle.addObserver(model)
        if (Services.p2p != null) ServiceForegroundConnector(this, model, RepeaterService::class)
        model.clients.observe(this) { clients ->
            val count = clients.count {
                it.ip.any { (ip, state) -> ip is Inet4Address && state == IpNeighbour.State.VALID }
            }
            badge.isVisible = count > 0
            badge.number = count
        }
        SmartSnackbar.Register(binding.fragmentHolder)
        WifiDoubleLock.ActivityListener(this)
        lifecycleScope.launch {
            BootReceiver.startIfEnabled()
            repeatOnLifecycle(Lifecycle.State.STARTED) {
                onAppUpdateAvailable(null)
                try {
                    UpdateChecker.check().collect(this@MainActivity::onAppUpdateAvailable)
                } catch (_: CancellationException) {
                } catch (e: AppUpdate.IgnoredException) {
                    Timber.d(e)
                } catch (e: Exception) {
                    Timber.w(e)
                    SmartSnackbar.make(e).show()
                }
            }
        }
    }

    private var lastUpdate: AppUpdate? = null
    private fun onAppUpdateAvailable(update: AppUpdate?) {
        lastUpdate = update
        updateItem.isVisible = update != null
        if (update == null) {
            updateItem.isEnabled = false
            return
        }
        updateItem.isEnabled = update.downloaded != false
        updateItem.setIcon(when (update.downloaded) {
            null -> R.drawable.ic_action_update
            false -> R.drawable.ic_file_downloading
            true -> R.drawable.ic_action_autorenew
        })
        updateItem.title = update.message ?: getText(R.string.title_update)
        updateBadge.isVisible = when (val days = update.stalenessDays) {
            null -> false
            else -> {
                if (days > 0) updateBadge.number = days else updateBadge.clearNumber()
                true
            }
        }
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
        R.id.navigation_update -> {
            lastUpdate!!.updateForResult(this, 1)
            false
        }
        else -> false
    }

    private fun displayFragment(fragment: Fragment) =
            supportFragmentManager.beginTransaction().replace(R.id.fragmentHolder, fragment).commitAllowingStateLoss()
}
