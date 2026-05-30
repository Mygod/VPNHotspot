package be.mygod.vpnhotspot.ui

import android.content.Context
import android.net.wifi.SoftApInfo
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
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.apInstanceIdentifierOrNull
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.ui.apconfiguration.VendorData
import be.mygod.vpnhotspot.ui.apconfiguration.formatTimeoutMillis
import kotlinx.coroutines.flow.catch
import timber.log.Timber
import java.text.NumberFormat
import java.util.Locale

internal enum class SoftApCallbackTarget { Tethered, LocalOnlyHotspot }

@RequiresApi(30)
@Composable
internal fun rememberWifiSummaryApi30(
    baseError: AnnotatedString?,
    linkStyles: TextLinkStyles,
    target: SoftApCallbackTarget = SoftApCallbackTarget.Tethered,
): State<AnnotatedString?> {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val locale = LocalConfiguration.current.locales[0]
    return produceState(baseError, context, lifecycleOwner, locale, baseError, linkStyles, target) {
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
        lifecycleOwner.lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) {
            (if (target == SoftApCallbackTarget.LocalOnlyHotspot && Build.VERSION.SDK_INT >= 33) {
                WifiApCommands.localOnlyHotspotSoftApCallbackFlow(expensive = true)
            } else WifiApCommands.softApCallbackFlow(expensive = true)).catch { e -> Timber.w(e) }.collect { event ->
                when (event) {
                    is WifiApManager.Event.OnStateChanged -> {
                        if (!WifiApManager.checkWifiApState(event.state)) return@collect
                        wifiFailureReason = if (event.state == WifiApManager.WIFI_AP_STATE_FAILED) {
                            event.failureReason
                        } else null
                        update()
                    }
                    is WifiApManager.Event.OnNumClientsChanged -> {
                        wifiNumClients = event.numClients
                        update()
                    }
                    is WifiApManager.Event.OnConnectedClientsChanged -> {
                        wifiNumClients = event.clients.size
                        update()
                    }
                    is WifiApManager.Event.OnInfoChanged -> {
                        wifiInfo = event.info
                        update()
                    }
                    is WifiApManager.Event.OnCapabilityChanged -> {
                        maxSupportedClients = event.capability.maxSupportedClients
                        update()
                    }
                    else -> { }
                }
            }
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
        if (Build.VERSION.SDK_INT >= 35) {
            VendorData.serialize(infos.flatMap { it.vendorData }).takeIf { it.isNotEmpty() }?.let { data ->
                if (length > 0) append('\n')
                append(context.getString(R.string.tethering_manage_wifi_vendor_data, data))
            }
        }
    }
    return if (summary.text.isEmpty()) null else summary
}
