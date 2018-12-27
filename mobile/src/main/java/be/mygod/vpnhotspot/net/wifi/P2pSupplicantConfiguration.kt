package be.mygod.vpnhotspot.net.wifi

import android.net.wifi.p2p.WifiP2pGroup
import android.os.Build
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.RootSession
import timber.log.Timber
import java.io.File

/**
 * This parser is based on:
 *   https://android.googlesource.com/platform/external/wpa_supplicant_8/+/d2986c2/wpa_supplicant/config.c#488
 *   https://android.googlesource.com/platform/external/wpa_supplicant_8/+/6fa46df/wpa_supplicant/config_file.c#182
 */
class P2pSupplicantConfiguration(private val group: WifiP2pGroup, ownerAddress: String?) {
    companion object {
        private val networkParser =
                "^(bssid=(([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2})|psk=(ext:|\"(.*)\"|[0-9a-fA-F]{64}\$))".toRegex()
        private val whitespaceMatcher = "\\s+".toRegex()
        private val confPath = if (Build.VERSION.SDK_INT >= 28)
            "/data/vendor/wifi/wpa/p2p_supplicant.conf" else "/data/misc/wifi/p2p_supplicant.conf"
    }

    private class NetworkBlock : ArrayList<String>() {
        var ssidLine: Int? = null
        var pskLine: Int? = null
        var psk: String? = null
        var groupOwner = false
        var bssidMatches = false

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
            val parser = Parser(it.execOutUnjoined("cat $confPath"))
            try {
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
                                if (match != null) if (match.groups[5] != null) {
                                    check(block.pskLine == null && block.psk == null)
                                    block.psk = match.groupValues[5].apply { check(length in 8..63) }
                                    block.pskLine = block.size
                                } else if (match.groups[2] != null &&
                                        match.groupValues[2].equals(group.owner.deviceAddress ?: ownerAddress, true)) {
                                    block.bssidMatches = true
                                }
                            }
                            block.add(parser.line)
                        }
                        block.add(parser.line)
                        result.add(block)
                        if (block.bssidMatches && block.groupOwner && target == null) { // keep first only
                            check(block.ssidLine != null && block.pskLine != null)
                            target = block
                        }
                    } else result.add(parser.line)
                }
                Pair(result, target!!)
            } catch (e: RuntimeException) {
                Timber.w("Failed to parse p2p_supplicant.conf, ownerAddress: $ownerAddress, P2P group: $group")
                Timber.w(parser.lines.joinToString("\n"))
                throw e
            }
        }
    }
    val psk = group.passphrase ?: content.second.psk!!

    fun update(ssid: String, psk: String) {
        val (lines, block) = content
        block[block.ssidLine!!] = "\tssid=" + ssid.toByteArray()
                .joinToString("") { (it.toInt() and 255).toString(16).padStart(2, '0') }
        block[block.pskLine!!] = "\tpsk=\"$psk\""   // no control chars or weird stuff
        val tempFile = File.createTempFile("vpnhotspot-", ".conf", app.cacheDir)
        try {
            tempFile.printWriter().use { writer ->
                lines.forEach { writer.println(it) }
            }
            // pkill not available on Lollipop. Source: https://android.googlesource.com/platform/system/core/+/master/shell_and_utilities/README.md
            RootSession.use {
                it.exec("cat ${tempFile.absolutePath} > $confPath")
                if (Build.VERSION.SDK_INT >= 23) it.exec("pkill wpa_supplicant") else {
                    val result = it.execOut("ps | grep wpa_supplicant").split(whitespaceMatcher)
                    check(result.size >= 2) { "wpa_supplicant not found, please toggle Airplane mode manually" }
                    it.exec("kill ${result[1]}")
                }
            }
        } finally {
            if (!tempFile.delete()) tempFile.deleteOnExit()
        }
    }
}
