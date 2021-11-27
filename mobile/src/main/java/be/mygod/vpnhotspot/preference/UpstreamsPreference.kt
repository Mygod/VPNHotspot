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
import be.mygod.vpnhotspot.util.allRoutes
import be.mygod.vpnhotspot.util.format
import be.mygod.vpnhotspot.util.parseNumericAddress
import timber.log.Timber

class UpstreamsPreference(context: Context, attrs: AttributeSet) : Preference(context, attrs),
        DefaultLifecycleObserver {
    companion object {
        private val internetV4Address = parseNumericAddress("8.8.8.8")
        private val internetV6Address = parseNumericAddress("2001:4860:4860::8888")
    }

    private open inner class Monitor : UpstreamMonitor.Callback {
        protected var currentInterfaces = emptyMap<String, Boolean>()
        val charSequence get() = currentInterfaces.map { (ifname, internet) ->
            if (internet) SpannableStringBuilder(ifname).apply {
                setSpan(StyleSpan(Typeface.BOLD), 0, length, 0)
            } else ifname
        }.joinTo(SpannableStringBuilder()).ifEmpty { "∅" }

        override fun onAvailable(properties: LinkProperties?) {
            val result = mutableMapOf<String, Boolean>()
            for (route in properties?.allRoutes ?: emptyList()) {
                result.compute(route.`interface` ?: continue) { _, internet ->
                    internet == true || try {
                        route.matches(internetV4Address) || route.matches(internetV6Address)
                    } catch (e: RuntimeException) {
                        Timber.w(e)
                        false
                    }
                }
            }
            currentInterfaces = result
            onUpdate()
        }
    }

    private val primary = Monitor()
    private val fallback: Monitor = object : Monitor() {
        override fun onFallback() {
            currentInterfaces = mapOf("<default>" to true)
            onUpdate()
        }
    }

    init {
        (context as LifecycleOwner).lifecycle.addObserver(this)
        onUpdate()
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
        summary = context.getText(R.string.settings_service_upstream_monitor_summary).format(
            context.resources.configuration.locale, primary.charSequence, fallback.charSequence)
    }
}
