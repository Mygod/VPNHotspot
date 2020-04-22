package be.mygod.vpnhotspot.net.wifi.configuration

import android.net.wifi.p2p.WifiP2pGroup
import android.os.Build
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.util.RootSession
import com.google.firebase.crashlytics.FirebaseCrashlytics
import java.io.File

/**
 * This parser is based on:
 *   https://android.googlesource.com/platform/external/wpa_supplicant_8/+/d2986c2/wpa_supplicant/config.c#488
 *   https://android.googlesource.com/platform/external/wpa_supplicant_8/+/6fa46df/wpa_supplicant/config_file.c#182
 */
class P2pSupplicantConfiguration(private val group: WifiP2pGroup, ownerAddress: String?) {
    companion object {
        private const val TAG = "P2pSupplicantConfiguration"
        private const val CONF_PATH_TREBLE = "/data/vendor/wifi/wpa/p2p_supplicant.conf"
        private const val CONF_PATH_LEGACY = "/data/misc/wifi/p2p_supplicant.conf"
        private val networkParser =
                "^(bssid=(([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2})|psk=(ext:|\"(.*)\"|[0-9a-fA-F]{64}\$)?)".toRegex()
        private val whitespaceMatcher = "\\s+".toRegex()
    }

    private class NetworkBlock : ArrayList<String>() {
        var ssidLine: Int? = null
        var pskLine: Int? = null
        var psk: String? = null
        var groupOwner = false
        var bssid: String? = null

        override fun toString() = joinToString("\n")
    }

    private class Parser(val lines: List<String>) {
        private val iterator = lines.iterator()
        lateinit var line: String
        lateinit var trimmed: String
        fun next() = if (iterator.hasNext()) {
            line = iterator.next().apply { trimmed = trimStart('\r', '\t', ' ') }
            true
        } else false
    }

    private val content by lazy {
        RootSession.use {
            val result = ArrayList<Any>()
            var target: NetworkBlock? = null
            val command = "cat $CONF_PATH_TREBLE || cat $CONF_PATH_LEGACY"
            val shell = it.execQuiet(command)
            RootSession.checkOutput(command, shell, false, false)
            val parser = Parser(shell.out)
            try {
                val bssids = listOfNotNull(group.owner.deviceAddress, ownerAddress, RepeaterService.lastMac)
                        .distinct()
                        .filter { it != "00:00:00:00:00:00" && it != "02:00:00:00:00:00" }
                while (parser.next()) {
                    if (parser.trimmed.startsWith("network={")) {
                        val block = NetworkBlock()
                        block.add(parser.line)
                        while (parser.next() && !parser.trimmed.startsWith('}')) {
                            if (parser.trimmed.startsWith("ssid=")) {
                                check(block.ssidLine == null)
                                block.ssidLine = block.size
                            } else if (parser.trimmed.startsWith("mode=3")) block.groupOwner = true else {
                                val match = networkParser.find(parser.trimmed)
                                if (match != null) match.groupValues[2].also { matchedBssid ->
                                    if (matchedBssid.isEmpty()) {
                                        check(block.pskLine == null && block.psk == null)
                                        if (match.groups[5] != null) {
                                            block.psk = match.groupValues[5].apply { check(length in 8..63) }
                                        }
                                        block.pskLine = block.size
                                    } else if (bssids.any { matchedBssid.equals(it, true) }) block.bssid = matchedBssid
                                }
                            }
                            block.add(parser.line)
                        }
                        block.add(parser.line)
                        result.add(block)
                        if (block.bssid != null && block.groupOwner && target == null) {    // keep first only
                            check(block.ssidLine != null && block.pskLine != null)
                            target = block
                        }
                    } else result.add(parser.line)
                }
                if (target == null && !RepeaterService.persistentSupported) {
                    result.add("")
                    result.add(NetworkBlock().apply {
                        // generate a basic network block, it is likely that vendor is going to add more stuff here
                        add("network={")
                        ssidLine = size
                        add("")
                        add("\tbssid=$bssid")
                        pskLine = size
                        add("")
                        add("\tproto=RSN")
                        add("\tkey_mgmt=WPA-PSK")
                        add("\tpairwise=CCMP")
                        add("\tauth_alg=OPEN")
                        add("\tmode=3")
                        add("\tdisabled=2")
                        add("}")
                        if (target == null) target = this
                    })
                }
                Triple(result, target!!, shell.err.isNotEmpty())
            } catch (e: RuntimeException) {
                FirebaseCrashlytics.getInstance().apply {
                    setCustomKey(TAG, parser.lines.joinToString("\n"))
                    setCustomKey("$TAG.ownerAddress", ownerAddress.toString())
                    setCustomKey("$TAG.p2pGroup", group.toString())
                }
                throw e
            }
        }
    }
    val psk = group.passphrase ?: content.second.psk!!

    fun update(ssid: String, psk: String) {
        val (lines, block, legacy) = content
        block[block.ssidLine!!] = "\tssid=" + ssid.toByteArray()
                .joinToString("") { (it.toInt() and 255).toString(16).padStart(2, '0') }
        block[block.pskLine!!] = "\tpsk=\"$psk\""   // no control chars or weird stuff
        val tempFile = File.createTempFile("vpnhotspot-", ".conf", app.deviceStorage.cacheDir)
        try {
            tempFile.printWriter().use { writer ->
                lines.forEach { writer.println(it) }
            }
            // pkill not available on Lollipop. Source: https://android.googlesource.com/platform/system/core/+/master/shell_and_utilities/README.md
            RootSession.use {
                it.exec("cat ${tempFile.absolutePath} > ${if (legacy) CONF_PATH_LEGACY else CONF_PATH_TREBLE}")
                if (Build.VERSION.SDK_INT >= 23) it.exec("pkill wpa_supplicant") else {
                    val result = try {
                        it.execOut("ps | grep wpa_supplicant").split(whitespaceMatcher).apply { check(size >= 2) }
                    } catch (e: Exception) {
                        throw IllegalStateException("wpa_supplicant not found, please toggle Airplane mode manually", e)
                    }
                    it.exec("kill ${result[1]}")
                }
            }
        } finally {
            if (!tempFile.delete()) tempFile.deleteOnExit()
        }
    }
}
