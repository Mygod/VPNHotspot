package be.mygod.vpnhotspot.net

import android.content.res.Resources
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import java.util.regex.Pattern

enum class TetherType {
    NONE, WIFI_P2P, USB, WIFI, WIMAX, BLUETOOTH;

    val icon get() = when (this) {
        USB -> R.drawable.ic_device_usb
        WIFI_P2P -> R.drawable.ic_action_settings_input_antenna
        WIFI, WIMAX -> R.drawable.ic_device_network_wifi
        BLUETOOTH -> R.drawable.ic_device_bluetooth
        else -> R.drawable.ic_device_wifi_tethering
    }

    companion object {
        private val usbRegexes: List<Pattern>
        private val wifiRegexes: List<Pattern>
        private val wimaxRegexes: List<Pattern>
        private val bluetoothRegexes: List<Pattern>

        /**
         * Source: https://android.googlesource.com/platform/frameworks/base/+/61fa313/core/res/res/values/config.xml#328
         */
        init {
            val appRes = app.resources
            val sysRes = Resources.getSystem()
            usbRegexes = appRes.getStringArray(sysRes
                    .getIdentifier("config_tether_usb_regexs", "array", "android"))
                    .map { it.toPattern() }
            wifiRegexes = appRes.getStringArray(sysRes
                    .getIdentifier("config_tether_wifi_regexs", "array", "android"))
                    .map { it.toPattern() }
            wimaxRegexes = appRes.getStringArray(sysRes
                    .getIdentifier("config_tether_wimax_regexs", "array", "android"))
                    .map { it.toPattern() }
            bluetoothRegexes = appRes.getStringArray(sysRes
                    .getIdentifier("config_tether_bluetooth_regexs", "array", "android"))
                    .map { it.toPattern() }
        }

        fun ofInterface(iface: String?, p2pDev: String? = null) = when {
            iface == null -> NONE
            iface == p2pDev -> WIFI_P2P
            usbRegexes.any { it.matcher(iface).matches() } -> USB
            wifiRegexes.any { it.matcher(iface).matches() } -> WIFI
            wimaxRegexes.any { it.matcher(iface).matches() } -> WIMAX
            bluetoothRegexes.any { it.matcher(iface).matches() } -> BLUETOOTH
            else -> NONE
        }
    }
}
