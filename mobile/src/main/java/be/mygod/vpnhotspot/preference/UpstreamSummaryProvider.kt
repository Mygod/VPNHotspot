package be.mygod.vpnhotspot.preference

import android.graphics.Typeface
import android.net.InetAddresses
import android.text.SpannableStringBuilder
import android.text.TextUtils
import android.text.style.StyleSpan
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.coroutineScope
import androidx.lifecycle.repeatOnLifecycle
import androidx.preference.EditTextPreference
import androidx.preference.Preference
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.monitor.Upstream
import be.mygod.vpnhotspot.util.allRoutes
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.launch
import timber.log.Timber

class UpstreamSummaryProvider(private val preference: EditTextPreference, lifecycle: Lifecycle,
                              upstream: SharedFlow<Upstream?>) : Preference.SummaryProvider<EditTextPreference> {
    companion object {
        private val internetV4Address = InetAddresses.parseNumericAddress("8.8.8.8")
        private val internetV6Address = InetAddresses.parseNumericAddress("2001:4860:4860::8888")
    }

    val fallback = preference.summary
    private var current: Upstream? = null
        set(value) {
            field = value
            preference.summaryProvider = this
        }

    init {
        lifecycle.coroutineScope.launch {
            lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) { upstream.collect { current = it } }
        }
        preference.summaryProvider = this
    }

    override fun provideSummary(preference: EditTextPreference) = TextUtils.expandTemplate(
        preference.context.getText(R.string.settings_upstream_current_summary),
        preference.text.let { if (it.isNullOrEmpty()) fallback else it } ?: "",
        SpannableStringBuilder().apply {
            val interfaces = mutableMapOf<String, Boolean>()
            for (route in current?.properties?.allRoutes ?: emptyList()) {
                interfaces.compute(route.`interface` ?: continue) { _, internet ->
                    internet == true || try {
                        route.matches(internetV4Address) || route.matches(internetV6Address)
                    } catch (e: RuntimeException) {
                        Timber.w(e)
                        false
                    }
                }
            }
            if (interfaces.isEmpty()) append('\u2205') else {
                var first = true
                for ((ifname, internet) in interfaces) {
                    if (first) first = false else append(", ")
                    val start = length
                    append(ifname)
                    if (internet) setSpan(StyleSpan(Typeface.BOLD), start, length, 0)
                }
            }
        },
    )
}
