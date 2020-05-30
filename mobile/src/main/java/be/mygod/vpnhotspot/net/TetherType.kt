package be.mygod.vpnhotspot.net

import android.content.res.Resources
import androidx.annotation.RequiresApi
import androidx.core.os.BuildCompat
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.util.Event0
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

    companion object : TetheringManager.TetheringEventCallback {
        private lateinit var usbRegexs: List<Pattern>
        private lateinit var wifiRegexs: List<Pattern>
        private var wifiP2pRegexs = emptyList<Pattern>()
        private val wimaxRegexs: List<Pattern>
        private lateinit var bluetoothRegexs: List<Pattern>
        private var ncmRegexs = emptyList<Pattern>()
        private val ethernetRegex: Pattern?
        private var requiresUpdate = true

        @RequiresApi(30)    // unused on lower APIs
        val listener = Event0()

        private fun Pair<String?, Resources>.getRegexs(name: String) = second
                .getStringArray(second.getIdentifier(name, "array", first))
                .filterNotNull()
                .map { it.toPattern() }

        @RequiresApi(30)
        private fun updateRegexs() {
            requiresUpdate = false
            val tethering = TetheringManager.PACKAGE to app.packageManager.getResourcesForApplication(
                    TetheringManager.resolvedService.serviceInfo.applicationInfo)
            usbRegexs = tethering.getRegexs("config_tether_usb_regexs")
            wifiRegexs = tethering.getRegexs("config_tether_wifi_regexs")
            wifiP2pRegexs = tethering.getRegexs("config_tether_wifi_p2p_regexs")
            bluetoothRegexs = tethering.getRegexs("config_tether_bluetooth_regexs")
            ncmRegexs = tethering.getRegexs("config_tether_ncm_regexs")
        }

        @RequiresApi(30)
        override fun onTetherableInterfaceRegexpsChanged() {
            requiresUpdate = true
            listener()
        }

        /**
         * Source: https://android.googlesource.com/platform/frameworks/base/+/32e772f/packages/Tethering/src/com/android/networkstack/tethering/TetheringConfiguration.java#93
         */
        init {
            val system = "android" to Resources.getSystem()
            if (BuildCompat.isAtLeastR()) {
                TetheringManager.registerTetheringEventCallback(null, this)
                updateRegexs()
            } else {
                usbRegexs = system.getRegexs("config_tether_usb_regexs")
                wifiRegexs = system.getRegexs("config_tether_wifi_regexs")
                bluetoothRegexs = system.getRegexs("config_tether_bluetooth_regexs")
            }
            wimaxRegexs = system.getRegexs("config_tether_wimax_regexs")
            // available since Android 4.0: https://android.googlesource.com/platform/frameworks/base/+/c96a667162fab44a250503caccb770109a9cb69a
            ethernetRegex = system.second.getString(system.second.getIdentifier(
                    "config_ethernet_iface_regex", "string", system.first)).run { if (isEmpty()) null else toPattern() }
        }

        /**
         * The result could change for the same interface since API 30+.
         * It will be triggered by [TetheringManager.TetheringEventCallback.onTetherableInterfaceRegexpsChanged].
         *
         * Based on: https://android.googlesource.com/platform/frameworks/base/+/5d36f01/packages/Tethering/src/com/android/networkstack/tethering/Tethering.java#479
         */
        tailrec fun ofInterface(iface: String?, p2pDev: String? = null): TetherType = when {
            iface == null -> NONE
            iface == p2pDev -> WIFI_P2P
            requiresUpdate -> {
                if (BuildCompat.isAtLeastR()) updateRegexs() else error("unexpected requiresUpdate")
                ofInterface(iface, p2pDev)
            }
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
