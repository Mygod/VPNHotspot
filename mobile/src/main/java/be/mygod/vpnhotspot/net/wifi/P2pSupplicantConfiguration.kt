package be.mygod.vpnhotspot.net.wifi

import android.net.wifi.WifiConfiguration
import android.os.Build
import android.util.Log
import android.widget.Toast
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.loggerSu
import be.mygod.vpnhotspot.noisySu
import java.io.File

class P2pSupplicantConfiguration {
    companion object {
        private const val TAG = "P2pSupplicationConf"
        // format for ssid is much more complicated, therefore we are only trying to find the line
        private val ssidMatcher = "^[\\r\\t ]*ssid=".toRegex()
        private val pskParser = "^[\\r\\t ]*psk=\"(.*)\"\$".toRegex(RegexOption.MULTILINE)
    }

    private val content by lazy { loggerSu("cat /data/misc/wifi/p2p_supplicant.conf") }

    fun readPsk(): String? {
        return try {
            pskParser.findAll(content ?: return null).single().groupValues[1]
        } catch (e: Exception) {
            Log.w(TAG, content)
            e.printStackTrace()
            Toast.makeText(app, e.message, Toast.LENGTH_LONG).show()
            null
        }
    }

    fun update(config: WifiConfiguration): Boolean? {
        val content = content ?: return null
        val tempFile = File.createTempFile("vpnhotspot-", ".conf", app.cacheDir)
        try {
            var ssidFound = false
            var pskFound = false
            tempFile.printWriter().use {
                for (line in content.lineSequence()) it.println(when {
                    ssidMatcher.containsMatchIn(line) -> {
                        ssidFound = true
                        "\tssid=" + config.SSID.toByteArray()
                                .joinToString("") { it.toInt().toString(16).padStart(2, '0') }
                    }
                    pskParser.containsMatchIn(line) -> {
                        pskFound = true
                        "\tpsk=\"${config.preSharedKey}\""  // no control chars or weird stuff
                    }
                    else -> line                            // do nothing
                })
            }
            if (!ssidFound || !pskFound) {
                Log.w(TAG, "Invalid conf ($ssidFound, $pskFound): $content")
                return false
            }
            // pkill not available on Lollipop. Source: https://android.googlesource.com/platform/system/core/+/master/shell_and_utilities/README.md
            return noisySu("cat ${tempFile.absolutePath} > /data/misc/wifi/p2p_supplicant.conf",
                    if (Build.VERSION.SDK_INT >= 23) "pkill wpa_supplicant"
                    else "set `ps | grep wpa_supplicant`; kill \$2")
        } finally {
            if (!tempFile.delete()) tempFile.deleteOnExit()
        }
    }
}
