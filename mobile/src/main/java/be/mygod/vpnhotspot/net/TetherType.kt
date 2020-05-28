package be.mygod.vpnhotspot.net

import android.content.res.Resources
import androidx.core.os.BuildCompat
import be.mygod.vpnhotspot.R
import java.util.regex.Pattern

enum class TetherType {
    NONE, WIFI_P2P, USB, WIFI, WIMAX, BLUETOOTH, NCM, ETHERNET;

    val icon get() = when (this) {
        USB -> R.drawable.ic_device_usb
        WIFI_P2P -> R.drawable.ic_action_settings_input_antenna
        WIFI -> R.drawable.ic_device_network_wifi
        WIMAX -> R.drawable.ic_action_contactless
        BLUETOOTH -> R.drawable.ic_device_bluetooth
        // if you have an issue with these Ethernet icon namings, blame Google
        NCM -> R.drawable.ic_action_settings_ethernet
        ETHERNET -> R.drawable.ic_content_inbox
        else -> R.drawable.ic_device_wifi_tethering
    }
    val isWifi get() = when (this) {
        WIFI_P2P, WIFI, WIMAX -> true
        else -> false
    }

    companion object {
        private val usbRegexs: List<Pattern>
        private val wifiRegexs: List<Pattern>
        private val wifiP2pRegexs: List<Pattern>
        private val wimaxRegexs: List<Pattern>
        private val bluetoothRegexs: List<Pattern>
        private val ncmRegexs: List<Pattern>
        private val ethernetRegex: Pattern?

        /**
         * Source: https://android.googlesource.com/platform/frameworks/base/+/32e772f/packages/Tethering/src/com/android/networkstack/tethering/TetheringConfiguration.java#93
         */
        init {
            val system = Resources.getSystem()
            fun getRegexs(name: String) = system
                    .getStringArray(system.getIdentifier(name, "array", "android"))
                    .filterNotNull()
                    .map { it.toPattern() }
            usbRegexs = getRegexs("config_tether_usb_regexs")
            wifiRegexs = getRegexs("config_tether_wifi_regexs")
            wifiP2pRegexs = if (BuildCompat.isAtLeastR()) getRegexs("config_tether_wifi_p2p_regexs") else emptyList()
            wimaxRegexs = getRegexs("config_tether_wimax_regexs")
            bluetoothRegexs = getRegexs("config_tether_bluetooth_regexs")
            ncmRegexs = if (BuildCompat.isAtLeastR()) getRegexs("config_tether_ncm_regexs") else emptyList()
            // available since Android 4.0: https://android.googlesource.com/platform/frameworks/base/+/c96a667162fab44a250503caccb770109a9cb69a
            ethernetRegex = system.getString(system.getIdentifier("config_ethernet_iface_regex", "string",
                    "android")).run { if (isEmpty()) null else toPattern() }
        }

        /**
         * Based on: https://android.googlesource.com/platform/frameworks/base/+/5d36f01/packages/Tethering/src/com/android/networkstack/tethering/Tethering.java#479
         */
        fun ofInterface(iface: String?, p2pDev: String? = null) = when {
            iface == null -> NONE
            iface == p2pDev -> WIFI_P2P
            wifiRegexs.any { it.matcher(iface).matches() } -> WIFI
            wifiP2pRegexs.any { it.matcher(iface).matches() } -> WIFI_P2P
            usbRegexs.any { it.matcher(iface).matches() } -> USB
            bluetoothRegexs.any { it.matcher(iface).matches() } -> BLUETOOTH
            ncmRegexs.any { it.matcher(iface).matches() } -> NCM
            wimaxRegexs.any { it.matcher(iface).matches() } -> WIMAX
            ethernetRegex?.matcher(iface)?.matches() == true -> ETHERNET
            else -> NONE
        }
    }
}
