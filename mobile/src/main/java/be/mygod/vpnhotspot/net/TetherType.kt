package be.mygod.vpnhotspot.net

import android.content.res.Resources
import android.os.Build
import androidx.annotation.DrawableRes
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.util.Event0
import be.mygod.vpnhotspot.util.findIdentifier
import timber.log.Timber
import java.util.regex.Pattern

enum class TetherType(@DrawableRes val icon: Int) {
    NONE(R.drawable.ic_device_wifi_tethering),
    WIFI_P2P(R.drawable.ic_action_settings_input_antenna),
    USB(R.drawable.ic_device_usb),
    WIFI(R.drawable.ic_device_network_wifi),
    BLUETOOTH(R.drawable.ic_device_bluetooth),
    // if you have an issue with these Ethernet icon namings, blame Google
    NCM(R.drawable.ic_action_settings_ethernet),
    ETHERNET(R.drawable.ic_content_inbox),
    WIGIG(R.drawable.ic_image_flash_on),
    ;

    val isWifi get() = when (this) {
        WIFI_P2P, WIFI, WIGIG -> true
        else -> false
    }

    companion object : TetheringManager.TetheringEventCallback {
        private lateinit var usbRegexs: List<Pattern>
        private lateinit var wifiRegexs: List<Pattern>
        private var wigigRegexs = emptyList<Pattern>()
        private var wifiP2pRegexs = emptyList<Pattern>()
        private lateinit var bluetoothRegexs: List<Pattern>
        private var ncmRegexs = emptyList<Pattern>()
        private val ethernetRegex: Pattern?
        private var requiresUpdate = false

        @RequiresApi(30)    // unused on lower APIs
        val listener = Event0()

        private fun Pair<String, Resources>.getRegexs(name: String, alternativePackage: String? = null): List<Pattern> {
            val id = second.findIdentifier(name, "array", first, alternativePackage)
            return if (id == 0) {
                if (name == "config_tether_wigig_regexs") Timber.i("$name is empty") else Timber.w(Exception(name))
                emptyList()
            } else try {
                second.getStringArray(id).filterNotNull().map { it.toPattern() }
            } catch (_: Resources.NotFoundException) {
                Timber.w(Exception("$name not found"))
                emptyList()
            }
        }

        @RequiresApi(30)
        private fun updateRegexs() = synchronized(this) {
            if (!requiresUpdate) return@synchronized
            requiresUpdate = false
            TetheringManager.registerTetheringEventCallback(null, this)
            val info = TetheringManager.resolvedService.serviceInfo
            val tethering = "com.android.networkstack.tethering" to
                    app.packageManager.getResourcesForApplication(info.applicationInfo)
            usbRegexs = tethering.getRegexs("config_tether_usb_regexs", info.packageName)
            wifiRegexs = tethering.getRegexs("config_tether_wifi_regexs", info.packageName)
            wigigRegexs = tethering.getRegexs("config_tether_wigig_regexs", info.packageName)
            wifiP2pRegexs = tethering.getRegexs("config_tether_wifi_p2p_regexs", info.packageName)
            bluetoothRegexs = tethering.getRegexs("config_tether_bluetooth_regexs", info.packageName)
            ncmRegexs = tethering.getRegexs("config_tether_ncm_regexs", info.packageName)
        }

        @RequiresApi(30)
        override fun onTetherableInterfaceRegexpsChanged(args: Array<out Any?>?) = synchronized(this) {
            if (requiresUpdate) return@synchronized
            Timber.i("onTetherableInterfaceRegexpsChanged: ${args?.contentDeepToString()}")
            TetheringManager.unregisterTetheringEventCallback(this)
            requiresUpdate = true
            listener()
        }

        /**
         * Source: https://android.googlesource.com/platform/frameworks/base/+/32e772f/packages/Tethering/src/com/android/networkstack/tethering/TetheringConfiguration.java#93
         */
        init {
            val system = "android" to Resources.getSystem()
            if (Build.VERSION.SDK_INT >= 30) requiresUpdate = true else {
                usbRegexs = system.getRegexs("config_tether_usb_regexs")
                wifiRegexs = system.getRegexs("config_tether_wifi_regexs")
                bluetoothRegexs = system.getRegexs("config_tether_bluetooth_regexs")
            }
            // available since Android 4.0: https://android.googlesource.com/platform/frameworks/base/+/c96a667162fab44a250503caccb770109a9cb69a
            ethernetRegex = try {
                system.second.getString(system.second.getIdentifier("config_ethernet_iface_regex", "string",
                        system.first)).run { if (isEmpty()) null else toPattern() }
            } catch (_: Resources.NotFoundException) {
                null
            }
        }

        /**
         * The result could change for the same interface since API 30+.
         * It will be triggered by [TetheringManager.TetheringEventCallback.onTetherableInterfaceRegexpsChanged].
         *
         * Based on: https://android.googlesource.com/platform/frameworks/base/+/5d36f01/packages/Tethering/src/com/android/networkstack/tethering/Tethering.java#479
         */
        fun ofInterface(iface: String?, p2pDev: String? = null) = synchronized(this) { ofInterfaceImpl(iface, p2pDev) }
        private tailrec fun ofInterfaceImpl(iface: String?, p2pDev: String?): TetherType = when {
            iface == null -> NONE
            iface == p2pDev -> WIFI_P2P
            requiresUpdate -> {
                if (Build.VERSION.SDK_INT >= 30) updateRegexs() else error("unexpected requiresUpdate")
                ofInterfaceImpl(iface, p2pDev)
            }
            wifiRegexs.any { it.matcher(iface).matches() } -> WIFI
            wigigRegexs.any { it.matcher(iface).matches() } -> WIGIG
            wifiP2pRegexs.any { it.matcher(iface).matches() } -> WIFI_P2P
            usbRegexs.any { it.matcher(iface).matches() } -> USB
            bluetoothRegexs.any { it.matcher(iface).matches() } -> BLUETOOTH
            ncmRegexs.any { it.matcher(iface).matches() } -> NCM
            ethernetRegex?.matcher(iface)?.matches() == true -> ETHERNET
            else -> NONE
        }
    }
}
