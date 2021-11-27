package be.mygod.vpnhotspot.preference

import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.view.View
import android.view.ViewGroup
import android.widget.ArrayAdapter
import androidx.core.os.bundleOf
import androidx.lifecycle.lifecycleScope
import androidx.preference.EditTextPreferenceDialogFragmentCompat
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.allInterfaceNames
import be.mygod.vpnhotspot.util.globalNetworkRequestBuilder
import be.mygod.vpnhotspot.widget.AlwaysAutoCompleteEditText

class AutoCompleteNetworkPreferenceDialogFragment : EditTextPreferenceDialogFragmentCompat() {
    fun setArguments(key: String) {
        arguments = bundleOf(ARG_KEY to key)
    }

    private lateinit var editText: AlwaysAutoCompleteEditText
    private lateinit var adapter: ArrayAdapter<String>
    private fun updateAdapter() {
        adapter.clear()
        adapter.addAll(interfaceNames.flatMap { it.value })
    }

    private val interfaceNames = mutableMapOf<Network, List<String>>()
    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
            interfaceNames[network] = properties.allInterfaceNames
            lifecycleScope.launchWhenStarted { updateAdapter() }
        }

        override fun onLost(network: Network) {
            interfaceNames.remove(network)
            lifecycleScope.launchWhenStarted { updateAdapter() }
        }
    }

    override fun onCreateDialogView(context: Context) = super.onCreateDialogView(context)!!.apply {
        editText = AlwaysAutoCompleteEditText(context).apply {
            id = android.R.id.edit
            minHeight = resources.getDimensionPixelSize(R.dimen.touch_target_min)
        }
        val oldEditText = findViewById<View>(android.R.id.edit)!!
        val container = oldEditText.parent as ViewGroup
        container.removeView(oldEditText)
        container.addView(editText, oldEditText.layoutParams)
    }

    override fun onBindDialogView(view: View) {
        super.onBindDialogView(view)
        editText.hint = (preference.summaryProvider as SummaryFallbackProvider).fallback
        adapter = ArrayAdapter(view.context, android.R.layout.select_dialog_item)
        editText.setAdapter(adapter)
        editText.clearFocus()   // having focus is buggy currently
    }

    override fun onStart() {
        super.onStart()
        Services.registerNetworkCallbackCompat(globalNetworkRequestBuilder().apply {
            removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
            removeCapability(NetworkCapabilities.NET_CAPABILITY_TRUSTED)
            removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
        }.build(), callback)
    }

    override fun onStop() {
        Services.connectivity.unregisterNetworkCallback(callback)
        interfaceNames.clear()
        updateAdapter()
        super.onStop()
    }
}
