package be.mygod.vpnhotspot.net

import android.content.res.Resources
import be.mygod.vpnhotspot.R
import java.util.regex.Pattern

enum class TetherType {
    NONE, WIFI_P2P, USB, WIFI, WIMAX, BLUETOOTH;

    val icon get() = when (this) {
        USB -> R.drawable.ic_device_usb
        WIFI_P2P -> R.drawable.ic_action_settings_input_antenna
        WIFI -> R.drawable.ic_device_network_wifi
        WIMAX -> R.drawable.ic_action_contactless
        BLUETOOTH -> R.drawable.ic_device_bluetooth
        else -> R.drawable.ic_device_wifi_tethering
    }
    val isWifi get() = when (this) {
        WIFI_P2P, WIFI, WIMAX -> true
        else -> false
    }

    companion object {
        private val usbRegexs: List<Pattern>
        private val wifiRegexs: List<Pattern>
        private val wimaxRegexs: List<Pattern>
        private val bluetoothRegexs: List<Pattern>

        /**
         * Source: https://android.googlesource.com/platform/frameworks/base/+/61fa313/core/res/res/values/config.xml#328
         */
        init {
            val system = Resources.getSystem()
            usbRegexs = system.getStringArray(system
                    .getIdentifier("config_tether_usb_regexs", "array", "android"))
                    .filterNotNull()
                    .map { it.toPattern() }
            wifiRegexs = system.getStringArray(system
                    .getIdentifier("config_tether_wifi_regexs", "array", "android"))
                    .filterNotNull()
                    .map { it.toPattern() }
            wimaxRegexs = system.getStringArray(system
                    .getIdentifier("config_tether_wimax_regexs", "array", "android"))
                    .filterNotNull()
                    .map { it.toPattern() }
            bluetoothRegexs = system.getStringArray(system
                    .getIdentifier("config_tether_bluetooth_regexs", "array", "android"))
                    .filterNotNull()
                    .map { it.toPattern() }
        }

        /**
         * Based on: https://android.googlesource.com/platform/frameworks/base/+/0e3d092/services/core/java/com/android/server/connectivity/Tethering.java#311
         */
        fun ofInterface(iface: String?, p2pDev: String? = null) = when {
            iface == null -> NONE
            iface == p2pDev -> WIFI_P2P
            wifiRegexs.any { it.matcher(iface).matches() } -> WIFI
            usbRegexs.any { it.matcher(iface).matches() } -> USB
            bluetoothRegexs.any { it.matcher(iface).matches() } -> BLUETOOTH
            wimaxRegexs.any { it.matcher(iface).matches() } -> WIMAX
            else -> NONE
        }
    }
}
