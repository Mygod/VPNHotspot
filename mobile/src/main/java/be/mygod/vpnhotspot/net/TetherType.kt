package be.mygod.vpnhotspot.net

import android.content.res.Resources
import be.mygod.vpnhotspot.App
import be.mygod.vpnhotspot.R

enum class TetherType {
    NONE, WIFI_P2P, USB, WIFI, WIMAX, BLUETOOTH;

    val icon get() = when (this) {
        USB -> R.drawable.ic_device_usb
        WIFI_P2P, WIFI, WIMAX -> R.drawable.ic_device_network_wifi
        BLUETOOTH -> R.drawable.ic_device_bluetooth
        else -> R.drawable.ic_device_wifi_tethering
    }

    companion object {
        /**
         * Source: https://android.googlesource.com/platform/frameworks/base/+/61fa313/core/res/res/values/config.xml#328
         */
        private val usbRegexes = App.app.resources.getStringArray(Resources.getSystem()
                .getIdentifier("config_tether_usb_regexs", "array", "android"))
                .map { it.toPattern() }
        private val wifiRegexes = App.app.resources.getStringArray(Resources.getSystem()
                .getIdentifier("config_tether_wifi_regexs", "array", "android"))
                .map { it.toPattern() }
        private val wimaxRegexes = App.app.resources.getStringArray(Resources.getSystem()
                .getIdentifier("config_tether_wimax_regexs", "array", "android"))
                .map { it.toPattern() }
        private val bluetoothRegexes = App.app.resources.getStringArray(Resources.getSystem()
                .getIdentifier("config_tether_bluetooth_regexs", "array", "android"))
                .map { it.toPattern() }

        fun ofInterface(iface: String, p2pDev: String? = null) = when {
            iface == p2pDev -> WIFI_P2P
            usbRegexes.any { it.matcher(iface).matches() } -> USB
            wifiRegexes.any { it.matcher(iface).matches() } -> WIFI
            wimaxRegexes.any { it.matcher(iface).matches() } -> WIMAX
            bluetoothRegexes.any { it.matcher(iface).matches() } -> BLUETOOTH
            else -> NONE
        }
    }
}
