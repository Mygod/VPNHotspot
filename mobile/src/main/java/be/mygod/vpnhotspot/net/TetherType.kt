package be.mygod.vpnhotspot.net

import android.net.ConnectivityManager
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R

enum class TetherType(val icon: Int, val isWifi: Boolean = false) {
    NONE(R.drawable.ic_device_wifi_tethering),
    WIFI_P2P(R.drawable.ic_action_settings_input_antenna, true),
    USB(R.drawable.ic_device_usb),
    WIFI(R.drawable.ic_device_network_wifi, true),
    BLUETOOTH(R.drawable.ic_device_bluetooth);

    companion object {
        private fun getRegexs(type: String) =
                (ConnectivityManager::class.java.getDeclaredMethod("getTetherable${type}Regexs")
                        .invoke(app.connectivity) as Array<String?>)
                        .filterNotNull()
                        .map { it.toPattern() }
        private val usbRegexs = getRegexs("Usb")
        private val wifiRegexs = getRegexs("Wifi")
        private val bluetoothRegexs = getRegexs("Bluetooth")

        /**
         * Based on: https://android.googlesource.com/platform/frameworks/base/+/0e3d092/services/core/java/com/android/server/connectivity/Tethering.java#311
         */
        fun ofInterface(iface: String?, p2pDev: String? = null) = when {
            iface == null -> NONE
            iface == p2pDev -> WIFI_P2P
            wifiRegexs.any { it.matcher(iface).matches() } -> WIFI
            usbRegexs.any { it.matcher(iface).matches() } -> USB
            bluetoothRegexs.any { it.matcher(iface).matches() } -> BLUETOOTH
            else -> NONE
        }
    }
}
