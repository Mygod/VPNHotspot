package be.mygod.vpnhotspot.net.wifi

import android.net.wifi.WifiConfiguration
import android.os.Build
import android.util.Log
import android.widget.Toast
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.loggerSu
import be.mygod.vpnhotspot.util.noisySu
import com.crashlytics.android.Crashlytics
import java.io.File

class P2pSupplicantConfiguration {
    companion object {
        private const val TAG = "P2pSupplicationConf"
        /**
         * Format for ssid is much more complicated, therefore we are only trying to find the line and rely on
         * Android's results instead.
         *
         * Source: https://android.googlesource.com/platform/external/wpa_supplicant_8/+/2933359/src/utils/common.c#631
         */
        private val ssidMatcher = "^[\\r\\t ]*ssid=".toRegex()
        /**
         * PSK parser can be found here: https://android.googlesource.com/platform/external/wpa_supplicant_8/+/d2986c2/wpa_supplicant/config.c#488
         */
        private val pskParser = "^[\\r\\t ]*psk=(ext:|\"(.*)\"|\"(.*)|[0-9a-fA-F]{64}\$)".toRegex(RegexOption.MULTILINE)
    }

    private val content by lazy { loggerSu("cat /data/misc/wifi/p2p_supplicant.conf") }

    fun readPsk(): String? {
        return try {
            val match = pskParser.findAll(content ?: return null).single()
            val result = match.groupValues[2] + match.groupValues[3]    // only one will match and hold non-empty value
            check(result.length in 8..63)
            result
        } catch (e: RuntimeException) {
            Crashlytics.log(Log.WARN, TAG, content)
            e.printStackTrace()
            Crashlytics.logException(e)
            Toast.makeText(app, e.message, Toast.LENGTH_LONG).show()
            null
        }
    }

    fun update(config: WifiConfiguration): Boolean? {
        val content = content ?: return null
        val tempFile = File.createTempFile("vpnhotspot-", ".conf", app.cacheDir)
        try {
            var ssidFound = 0
            var pskFound = 0
            tempFile.printWriter().use {
                for (line in content.lineSequence()) it.println(when {
                    ssidMatcher.containsMatchIn(line) -> {
                        ssidFound += 1
                        "\tssid=" + config.SSID.toByteArray()
                                .joinToString("") { (it.toInt() and 255).toString(16).padStart(2, '0') }
                    }
                    pskParser.containsMatchIn(line) -> {
                        pskFound += 1
                        "\tpsk=\"${config.preSharedKey}\""  // no control chars or weird stuff
                    }
                    else -> line                            // do nothing
                })
            }
            if (ssidFound != 1 || pskFound != 1) {
                Crashlytics.log(Log.WARN, TAG, "Invalid conf ($ssidFound, $pskFound): $content")
            }
            if (ssidFound == 0 || pskFound == 0) return false
            // pkill not available on Lollipop. Source: https://android.googlesource.com/platform/system/core/+/master/shell_and_utilities/README.md
            return noisySu("cat ${tempFile.absolutePath} > /data/misc/wifi/p2p_supplicant.conf",
                    if (Build.VERSION.SDK_INT >= 23) "pkill wpa_supplicant"
                    else "set `ps | grep wpa_supplicant`; kill \$2")
        } finally {
            if (!tempFile.delete()) tempFile.deleteOnExit()
        }
    }
}
