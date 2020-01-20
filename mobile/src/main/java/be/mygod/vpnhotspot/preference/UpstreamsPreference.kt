package be.mygod.vpnhotspot.preference

import android.content.Context
import android.graphics.Typeface
import android.net.LinkProperties
import android.text.SpannableStringBuilder
import android.text.style.StyleSpan
import android.util.AttributeSet
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.lifecycleScope
import androidx.preference.Preference
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.monitor.FallbackUpstreamMonitor
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.util.SpanFormatter
import be.mygod.vpnhotspot.util.parseNumericAddress

class UpstreamsPreference(context: Context, attrs: AttributeSet) : Preference(context, attrs),
        DefaultLifecycleObserver {
    companion object {
        private val internetAddress = parseNumericAddress("8.8.8.8")
    }

    private data class Interface(val ifname: String, val internet: Boolean)
    private inner class Monitor : UpstreamMonitor.Callback {
        private var currentInterface: Interface? = null
        val charSequence get() = currentInterface?.run {
            if (internet) SpannableStringBuilder(ifname).apply {
                setSpan(StyleSpan(Typeface.BOLD), 0, length, 0)
            } else ifname
        } ?: "∅"

        override fun onAvailable(ifname: String, properties: LinkProperties) {
            currentInterface = Interface(ifname, properties.routes.any { it.matches(internetAddress) })
            onUpdate()
        }

        override fun onLost() {
            currentInterface = null
            onUpdate()
        }
    }

    private val primary = Monitor()
    private val fallback = Monitor()

    init {
        (context as LifecycleOwner).lifecycle.addObserver(this)
    }

    override fun onStart(owner: LifecycleOwner) {
        UpstreamMonitor.registerCallback(primary)
        FallbackUpstreamMonitor.registerCallback(fallback)
    }
    override fun onStop(owner: LifecycleOwner) {
        UpstreamMonitor.unregisterCallback(primary)
        FallbackUpstreamMonitor.unregisterCallback(fallback)
    }

    private fun onUpdate() = (context as LifecycleOwner).lifecycleScope.launchWhenStarted {
        summary = SpanFormatter.format(context.getText(R.string.settings_service_upstream_monitor_summary),
                primary.charSequence, fallback.charSequence)
    }
}
