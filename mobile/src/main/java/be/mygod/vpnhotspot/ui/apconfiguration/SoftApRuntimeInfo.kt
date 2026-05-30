package be.mygod.vpnhotspot.ui.apconfiguration

import android.content.Context
import android.net.wifi.SoftApCapability
import android.net.wifi.SoftApConfiguration
import android.os.Build
import androidx.annotation.RequiresApi
import androidx.compose.runtime.Composable
import androidx.compose.runtime.State
import androidx.compose.runtime.produceState
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.ui.softApBandLabel
import be.mygod.vpnhotspot.ui.softApFeatureLabel
import be.mygod.vpnhotspot.util.RangeInput
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import be.mygod.vpnhotspot.util.toRegionalIndicatorFlagOrNull
import kotlinx.coroutines.flow.catch
import timber.log.Timber
import java.util.Locale

internal data class SoftApRuntimeInfo(
    val supportedChannels: String?,
    val advancedInfo: String,
)

@RequiresApi(30)
@Composable
internal fun rememberSoftApRuntimeInfo(): State<SoftApRuntimeInfo?> {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    return produceState<SoftApRuntimeInfo?>(null, context, lifecycleOwner) {
        var currentCapability: SoftApCapability? = null
        fun update() {
            value = currentCapability?.let {
                SoftApRuntimeInfo(
                    supportedChannels = softApSupportedChannels(context, it),
                    advancedInfo = softApAdvancedInfo(context, it),
                )
            }
        }
        lifecycleOwner.lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) {
            WifiApCommands.softApCallbackFlow(expensive = true).catch { e -> Timber.w(e) }.collect { event ->
                when (event) {
                    is WifiApManager.Event.OnCapabilityChanged -> {
                        currentCapability = event.capability
                        update()
                    }
                    else -> { }
                }
            }
        }
    }
}

private fun softApAdvancedInfo(context: Context, capability: SoftApCapability): String {
    val lines = mutableListOf(
        context.getString(R.string.repeater_features) + softApSupportedFeatures(context, capability),
    )
    if (Build.VERSION.SDK_INT >= 31) try {
        UnblockCentral.getCountryCode(capability)
    } catch (e: NoSuchMethodError) {
        if (Build.VERSION.SDK_INT >= 33) Timber.w(e)
        null
    }?.let { countryCode ->
        val label = countryCode.toRegionalIndicatorFlagOrNull()?.let { flag ->
            "${countryCode.uppercase(Locale.US)} $flag"
        } ?: countryCode
        lines += context.getString(R.string.tethering_manage_wifi_country_code, label)
    }
    return lines.joinToString("\n")
}

private fun softApSupportedFeatures(context: Context, capability: SoftApCapability): String {
    var features = 0L
    var probe = 1L
    while (probe != 0L) {
        if (capability.areFeaturesSupported(probe)) features = features or probe
        probe += probe
    }
    if (Build.VERSION.SDK_INT >= 31) for ((flag, band) in arrayOf(
        SoftApCapability.SOFTAP_FEATURE_BAND_24G_SUPPORTED to SoftApConfiguration.BAND_2GHZ,
        SoftApCapability.SOFTAP_FEATURE_BAND_5G_SUPPORTED to SoftApConfiguration.BAND_5GHZ,
        SoftApCapability.SOFTAP_FEATURE_BAND_6G_SUPPORTED to SoftApConfiguration.BAND_6GHZ,
        SoftApCapability.SOFTAP_FEATURE_BAND_60G_SUPPORTED to SoftApConfiguration.BAND_60GHZ,
    )) {
        if (capability.getSupportedChannelList(band).isNotEmpty()) features = features and flag.inv()
    }
    return sequence {
        if (WifiApManager.isApMacRandomizationSupported) yield(context.getString(
            R.string.tethering_manage_wifi_feature_ap_mac_randomization))
        if (Services.wifi.isStaApConcurrencySupported) yield(context.getString(
            R.string.tethering_manage_wifi_feature_sta_ap_concurrency))
        if (Build.VERSION.SDK_INT >= 31) {
            if (Services.wifi.isBridgedApConcurrencySupported) yield(context.getString(
                R.string.tethering_manage_wifi_feature_bridged_ap_concurrency))
            if (Services.wifi.isStaBridgedApConcurrencySupported) yield(context.getString(
                R.string.tethering_manage_wifi_feature_sta_bridged_ap_concurrency))
        }
        while (features != 0L) {
            val bit = features.takeLowestOneBit()
            yield(softApFeatureLabel(context, bit))
            features = features and bit.inv()
        }
    }.toList().ifEmpty {
        listOf(context.getString(R.string.tethering_manage_wifi_no_features))
    }.joinAsBullets()
}

private fun softApSupportedChannels(context: Context, capability: SoftApCapability): String? {
    if (Build.VERSION.SDK_INT < 31) return null
    val channels = buildList {
        for (band in SoftApConfigurationCompat.BAND_TYPES) {
            val list = capability.getSupportedChannelList(band)
            if (list.isNotEmpty()) {
                add("${softApBandLabel(context, band)} (${RangeInput.toString(list)})")
            }
        }
    }
    return if (channels.isEmpty()) null else context.getString(
        R.string.tethering_manage_wifi_supported_channels,
        channels.joinAsBullets(),
    ).trimStart()
}
