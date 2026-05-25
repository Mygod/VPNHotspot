package be.mygod.vpnhotspot.ui

import android.content.Context
import android.net.wifi.SoftApCapability
import android.net.wifi.SoftApInfo
import android.net.wifi.WifiClient
import android.net.wifi.`WifiManager$SoftApCallback`
import android.os.Build
import androidx.annotation.RequiresApi
import androidx.compose.runtime.Composable
import androidx.compose.runtime.State
import androidx.compose.runtime.produceState
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.buildAnnotatedString
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.compose.LocalLifecycleOwner
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.apInstanceIdentifierOrNull
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.ui.apconfiguration.formatTimeoutMillis
import timber.log.Timber
import java.text.NumberFormat
import java.util.Locale

@RequiresApi(30)
@Composable
internal fun rememberWifiSummaryApi30(
    baseError: AnnotatedString?,
    linkStyles: TextLinkStyles,
): State<AnnotatedString?> {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val locale = LocalConfiguration.current.locales[0]
    return produceState(baseError, context, lifecycleOwner, locale, baseError, linkStyles) {
        var wifiFailureReason: Int? = null
        var wifiNumClients: Int? = null
        var wifiInfo = emptyList<SoftApInfo>()
        var maxSupportedClients: Int? = null
        fun update() {
            value = wifiSummary(
                context,
                locale,
                wifiFailureReason,
                wifiNumClients,
                softApInfoSummary(context, locale, wifiInfo, linkStyles),
                maxSupportedClients,
                baseError,
            )
        }
        val callback = object : `WifiManager$SoftApCallback` {
            override fun onStateChanged(state: Int, failureReason: Int) {
                if (!WifiApManager.checkWifiApState(state)) return
                wifiFailureReason = if (state == WifiApManager.WIFI_AP_STATE_FAILED) failureReason else null
                update()
            }

            override fun onNumClientsChanged(numClients: Int) {
                wifiNumClients = numClients
                update()
            }

            override fun onConnectedClientsChanged(clients: List<WifiClient>) {
                wifiNumClients = clients.size
                update()
            }

            override fun onInfoChanged(info: SoftApInfo) {
                wifiInfo = if (info.frequency == 0 && info.bandwidth ==
                    SoftApInfo.CHANNEL_WIDTH_INVALID) emptyList() else listOf(info)
                update()
            }

            override fun onInfoChanged(info: List<SoftApInfo>) {
                wifiInfo = info
                update()
            }

            override fun onCapabilityChanged(capability: SoftApCapability) {
                maxSupportedClients = capability.maxSupportedClients
                update()
            }
        }
        var registered = false
        fun register() {
            if (!registered) {
                WifiApCommands.registerSoftApCallback(callback)
                registered = true
            }
        }
        fun unregister() {
            if (registered) {
                WifiApCommands.unregisterSoftApCallback(callback)
                registered = false
            }
        }
        val observer = object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) = register()
            override fun onStop(owner: LifecycleOwner) = unregister()
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        if (lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED)) register()
        awaitDispose {
            lifecycleOwner.lifecycle.removeObserver(observer)
            unregister()
        }
    }
}

private fun softApInfoSummary(
    context: Context,
    locale: Locale,
    infos: List<SoftApInfo>,
    linkStyles: TextLinkStyles,
): AnnotatedString? {
    val integerFormat = NumberFormat.getIntegerInstance(locale)
    val summary = buildAnnotatedString {
        for (info in infos) {
            if (length > 0) append('\n')
            val frequency = info.frequency
            val channel = SoftApConfigurationCompat.frequencyToChannel(frequency)
            val bandwidth = channelBandwidthLabel(context, info.bandwidth)
            if (Build.VERSION.SDK_INT >= 31) {
                val bssid = info.bssid?.toString()
                val apInstanceIdentifier = info.apInstanceIdentifierOrNull
                val bssidAp = bssid?.let { apInstanceIdentifier?.let { id -> "$it%$id" } ?: it }
                    ?: apInstanceIdentifier ?: "?"
                val timeout = info.autoShutdownTimeoutMillis
                val line = context.getString(if (timeout == 0L) {
                    R.string.tethering_manage_wifi_info_timeout_disabled
                } else R.string.tethering_manage_wifi_info_timeout_enabled,
                    integerFormat.format(frequency.toLong()),
                    integerFormat.format(channel.toLong()),
                    bandwidth,
                    bssidAp,
                    integerFormat.format(info.wifiStandard.toLong()),
                    formatTimeoutMillis(context, timeout),
                )
                if (bssid == null) {
                    append(line)
                } else {
                    val bssidStart = line.indexOf(bssid)
                    if (bssidStart < 0) append(line) else {
                        append(line.substring(0, bssidStart))
                        appendMacAddress(bssid, linkStyles)
                        append(line.substring(bssidStart + bssid.length))
                    }
                }
            } else append(context.getString(
                R.string.tethering_manage_wifi_info,
                integerFormat.format(frequency.toLong()),
                integerFormat.format(channel.toLong()),
                bandwidth,
            ))
            try {
                info.mldAddress
            } catch (e: NoSuchMethodError) {
                if (Build.VERSION.SDK_INT >= 36) Timber.w(e)
                null
            }?.let {
                append(", MLD MAC ")
                appendMacAddress(it.toString(), linkStyles)
            }
        }
    }
    return if (summary.text.isEmpty()) null else summary
}
