package be.mygod.vpnhotspot

import android.databinding.DataBindingUtil
import android.os.Bundle
import android.support.design.widget.BottomNavigationView
import android.support.v4.app.Fragment
import android.support.v7.app.AppCompatActivity
import android.view.MenuItem
import be.mygod.vpnhotspot.databinding.ActivityMainBinding

class MainActivity : AppCompatActivity(), BottomNavigationView.OnNavigationItemSelectedListener {
    private lateinit var binding: ActivityMainBinding

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        binding = DataBindingUtil.setContentView(this, R.layout.activity_main)
        binding.navigation.setOnNavigationItemSelectedListener(this)
        if (savedInstanceState == null) displayFragment(RepeaterFragment())
    }

    override fun onNavigationItemSelected(item: MenuItem) = when (item.itemId) {
        R.id.navigation_repeater -> {
            if (!item.isChecked) {
                item.isChecked = true
                displayFragment(RepeaterFragment())
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
                displayFragment(SettingsFragment())
            }
            true
        }
        else -> false
    }

    private fun displayFragment(fragment: Fragment) =
            supportFragmentManager.beginTransaction().replace(R.id.fragmentHolder, fragment).commit()
}
