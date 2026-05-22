package be.mygod.vpnhotspot.ui.apconfiguration

import android.content.Context
import android.icu.text.MeasureFormat
import android.icu.util.Measure
import android.icu.util.MeasureUnit
import android.net.MacAddress
import android.net.wifi.SoftApConfiguration
import android.os.Build
import android.util.Base64
import android.util.SparseIntArray
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.ui.autofill.ContentType
import androidx.compose.ui.text.input.KeyboardType
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.ui.channelBandwidthLabel
import be.mygod.vpnhotspot.ui.softApBandLabel
import be.mygod.vpnhotspot.util.RangeInput
import be.mygod.vpnhotspot.util.readableMessage
import timber.log.Timber
import java.text.DecimalFormat
import java.text.DecimalFormatSymbols

internal fun parseMacList(value: String) = value.split(NON_MAC_CHARS).filter { it.isNotEmpty() }.map(MacAddress::fromString)

internal fun validateOptionalLong(value: String, validate: (Long) -> Unit): String? {
    if (value.isEmpty()) return null
    return try {
        validate(value.toLong())
        null
    } catch (e: Exception) {
        e.readableMessage
    }
}

internal fun validateMacList(value: String): String? = try {
    parseMacList(value)
    null
} catch (e: IllegalArgumentException) {
    e.readableMessage
}

internal fun validateOptionalMac(value: String, validate: (MacAddress) -> Unit = {}): String? {
    if (value.isEmpty()) return null
    return try {
        validate(MacAddress.fromString(value))
        null
    } catch (e: Exception) {
        e.readableMessage
    }
}

internal fun validateAcsChannels(band: Int, value: String): String? = try {
    SoftApConfigurationCompat.testPlatformValidity(band, RangeInput.fromString(value).toIntArray())
    null
} catch (e: Exception) {
    e.readableMessage
}

internal fun timeoutSummary(context: Context, value: String, defaultMillis: Long): String {
    val millis = value.toLongOrNull()
    return if (millis == null || millis <= 0) {
        context.getString(R.string.wifi_hotspot_timeout_default, formatTimeoutMillis(context, defaultMillis))
    } else formatTimeoutMillis(context, millis)
}

internal fun formatTimeoutMillis(context: Context, millis: Long): String {
    val formatter = MeasureFormat.getInstance(
        context.resources.configuration.locales[0],
        MeasureFormat.FormatWidth.NUMERIC,
    )
    var remaining = millis.coerceAtLeast(0)
    val measures = ArrayList<Measure>(5)
    for ((unitMillis, unit) in arrayOf(
        86_400_000L to MeasureUnit.DAY,
        3_600_000L to MeasureUnit.HOUR,
        60_000L to MeasureUnit.MINUTE,
        1_000L to MeasureUnit.SECOND,
        1L to MeasureUnit.MILLISECOND,
    )) {
        val count = remaining / unitMillis
        if (count <= 0) continue
        measures += Measure(count, unit)
        remaining %= unitMillis
    }
    if (measures.isEmpty()) measures += Measure(0, MeasureUnit.MILLISECOND)
    return formatter.formatMeasures(*measures.toTypedArray())
}

internal fun ChannelOption.label(context: Context): String {
    if (this == ChannelOption.Disabled) return context.getString(R.string.wifi_ap_choose_disabled)
    return if (channel == 0) {
        val format = DecimalFormat("#.#", DecimalFormatSymbols.getInstance(context.resources.configuration.locales[0]))
        context.getString(R.string.wifi_ap_choose_G, arrayOf(
            SoftApConfiguration.BAND_2GHZ to 2.4,
            SoftApConfiguration.BAND_5GHZ to 5,
            SoftApConfiguration.BAND_6GHZ to 6,
            SoftApConfiguration.BAND_60GHZ to 60,
        ).filter { (mask, _) -> band and mask == mask }.joinToString("/") { (_, name) -> format.format(name) })
    } else "${SoftApConfigurationCompat.channelToFrequency(band, channel)} MHz ($channel)"
}

internal fun currentChannelOptions(p2pMode: Boolean): List<ChannelOption> = when {
    !p2pMode -> SOFT_AP_OPTIONS
    RepeaterService.safeMode -> P2P_SAFE_OPTIONS
    else -> P2P_UNSAFE_OPTIONS
}

internal fun locate(
    channels: SparseIntArray,
    index: Int,
    options: List<ChannelOption>,
    p2pMode: Boolean,
    pasted: Boolean = false,
): ChannelOption {
    val band = channels.keyAt(index)
    val channel = channels.valueAt(index)
    return options.firstOrNull { it.band == band && it.channel == channel } ?: run {
        val msg = "Unable to locate $band, $channel, ${p2pMode && !RepeaterService.safeMode}"
        if (pasted || p2pMode) Timber.w(msg) else Timber.w(Exception(msg))
        options.first()
    }
}

private fun genAutoOptions(band: Int) = (1..band).filter { it and band == it }.map { ChannelOption(it) }

internal data class SecurityOption(val label: String, val value: Int)

internal open class ChannelOption(val band: Int = 0, val channel: Int = 0) {
    object Disabled : ChannelOption(-1) {
        override fun toString() = app.getString(R.string.wifi_ap_choose_disabled)
    }

    override fun toString() = if (channel == 0) {
        softApBandLabel(app, band)
    } else "${SoftApConfigurationCompat.channelToFrequency(band, channel)} MHz ($channel)"
}

internal class BandWidth(val width: Int, val name: String = "") : Comparable<BandWidth> {
    override fun compareTo(other: BandWidth) = width - other.width
}

internal fun BandWidth.label(context: Context) = channelBandwidthLabel(context, width, name)

internal const val BASE64_FLAGS = Base64.NO_PADDING or Base64.NO_WRAP
internal val MACHINE_TEXT_KEYBOARD_OPTIONS = KeyboardOptions(
    autoCorrectEnabled = false,
    keyboardType = KeyboardType.Ascii,
)
internal val WIFI_SSID_CONTENT_TYPE = ContentType.NewUsername + ContentType.Username
internal val WIFI_PASSWORD_CONTENT_TYPE = ContentType("wifiPassword") + ContentType.Password
private val NON_MAC_CHARS = "[^0-9a-fA-F:]+".toRegex()
private val CHANNELS_2G = (1..14).map { ChannelOption(SoftApConfiguration.BAND_2GHZ, it) }
private val CHANNELS_6G by lazy {
    val c5g = CHANNELS_2G + (1..196).map { ChannelOption(SoftApConfiguration.BAND_5GHZ, it) }
    if (Build.VERSION.SDK_INT >= 30) c5g + (1..253).map { ChannelOption(SoftApConfiguration.BAND_6GHZ, it) }
    else c5g
}
private val P2P_UNSAFE_OPTIONS by lazy {
    listOf(ChannelOption(SoftApConfigurationCompat.BAND_LEGACY)) +
            CHANNELS_2G + (15..165).map { ChannelOption(SoftApConfiguration.BAND_5GHZ, it) }
}
private val P2P_SAFE_OPTIONS by lazy {
    (if (Build.VERSION.SDK_INT >= 36) listOf(
        ChannelOption(SoftApConfigurationCompat.BAND_ANY_30),
        ChannelOption(SoftApConfiguration.BAND_2GHZ),
        ChannelOption(SoftApConfiguration.BAND_5GHZ),
        ChannelOption(SoftApConfiguration.BAND_6GHZ),
    ) else genAutoOptions(SoftApConfigurationCompat.BAND_LEGACY)) + CHANNELS_6G
}
private val SOFT_AP_OPTIONS by lazy {
    if (Build.VERSION.SDK_INT >= 30) {
        genAutoOptions(SoftApConfigurationCompat.BAND_ANY_31) + CHANNELS_6G +
                (1..6).map { ChannelOption(SoftApConfiguration.BAND_60GHZ, it) }
    } else P2P_SAFE_OPTIONS
}
internal val SECURITY_TYPES_WITHOUT_PASSWORD = setOf(
    SoftApConfiguration.SECURITY_TYPE_OPEN,
    SoftApConfiguration.SECURITY_TYPE_WPA3_OWE_TRANSITION,
    SoftApConfiguration.SECURITY_TYPE_WPA3_OWE,
)
internal val P2P_SECURITY_TYPES = arrayOf("WPA2-Personal", "WPA3-Personal Compatibility Mode", "WPA3-Personal")
