package be.mygod.vpnhotspot

import android.databinding.DataBindingUtil
import android.os.Bundle
import android.support.v7.app.AppCompatActivity
import be.mygod.vpnhotspot.databinding.ActivitySettingsBinding

class SettingsActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        DataBindingUtil.setContentView<ActivitySettingsBinding>(this, R.layout.activity_settings)
                .toolbar.setNavigationOnClickListener({ navigateUp() })
    }
}
