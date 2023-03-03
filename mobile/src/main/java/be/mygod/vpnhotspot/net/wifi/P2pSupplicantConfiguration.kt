package be.mygod.vpnhotspot.net.wifi

import android.net.MacAddress
import android.net.wifi.p2p.WifiP2pGroup
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.root.RepeaterCommands
import be.mygod.vpnhotspot.root.RootManager
import com.google.firebase.crashlytics.FirebaseCrashlytics

/**
 * This parser is based on:
 *   https://android.googlesource.com/platform/external/wpa_supplicant_8/+/d2986c2/wpa_supplicant/config.c#488
 *   https://android.googlesource.com/platform/external/wpa_supplicant_8/+/6fa46df/wpa_supplicant/config_file.c#182
 */
class P2pSupplicantConfiguration(private val group: WifiP2pGroup? = null) {
    companion object {
        private const val TAG = "P2pSupplicantConfiguration"
        private const val PERSISTENT_MAC = "p2p_device_persistent_mac_addr="
        private val networkParser =
                "^(bssid=(([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2})|psk=(ext:|\"(.*)\"|[0-9a-fA-F]{64}\$)?)".toRegex()
    }

    private class NetworkBlock : ArrayList<String>() {
        var ssidLine: Int? = null
        var pskLine: Int? = null
        var bssidLine: Int? = null
        var psk: String? = null
        var groupOwner = false
        var bssid: String? = null

        override fun toString() = joinToString("\n")
    }

    private class Parser(val lines: Iterator<String>) {
        lateinit var line: String
        lateinit var trimmed: String
        fun next() = if (lines.hasNext()) {
            line = lines.next().apply { trimmed = trimStart('\r', '\t', ' ') }
            true
        } else false
    }

    private data class Content(val lines: ArrayList<Any>, var target: NetworkBlock, var persistentMacLine: Int?,
                               var legacy: Boolean)

    private lateinit var content: Content
    suspend fun init(ownerAddress: String? = null) {
        val result = ArrayList<Any>()
        var target: NetworkBlock? = null
        var persistentMacLine: Int? = null
        val (config, legacy) = RootManager.use { it.execute(RepeaterCommands.ReadP2pConfig()) }
        try {
            var bssids = listOfNotNull(group?.owner?.deviceAddress, ownerAddress)
                    .distinct()
                    .filter {
                        val mac = MacAddress.fromString(it)
                        try {
                            mac != MacAddressCompat.ALL_ZEROS_ADDRESS && mac != MacAddressCompat.ANY_ADDRESS
                        } catch (_: IllegalArgumentException) {
                            false
                        }
                    }
            val parser = Parser(config.lineSequence().iterator())
            while (parser.next()) {
                if (parser.trimmed.startsWith("network={")) {
                    val block = NetworkBlock()
                    block.add(parser.line)
                    while (parser.next() && !parser.trimmed.startsWith('}')) {
                        if (parser.trimmed.startsWith("ssid=")) {
                            check(block.ssidLine == null) { "Duplicated SSID" }
                            block.ssidLine = block.size
                        } else if (parser.trimmed.startsWith("mode=3")) block.groupOwner = true else {
                            val match = networkParser.find(parser.trimmed)
                            if (match != null) match.groupValues[2].also { matchedBssid ->
                                if (matchedBssid.isEmpty()) {
                                    check(block.pskLine == null && block.psk == null)
                                    if (match.groups[5] != null) {
                                        block.psk = match.groupValues[5].apply {
                                            when (length) {
                                                in 8..63 -> { }
                                                64 -> error("WPA-PSK hex not supported")
                                                else -> error("Unknown length $length")
                                            }
                                        }
                                    }
                                    block.pskLine = block.size
                                } else if (bssids.any { matchedBssid.equals(it, true) }) {
                                    block.bssid = matchedBssid
                                    block.bssidLine = block.size
                                }
                            }
                        }
                        block.add(parser.line)
                    }
                    block.add(parser.line)
                    result.add(block)
                    if (block.bssid != null && block.groupOwner && target == null) {    // keep first only
                        check(block.ssidLine != null && block.pskLine != null) { "Missing SSID/PSK" }
                        target = block
                    }
                } else {
                    if (parser.trimmed.startsWith(PERSISTENT_MAC)) {
                        require(persistentMacLine == null) { "Duplicated $PERSISTENT_MAC" }
                        persistentMacLine = result.size
                        bssids = listOf(parser.trimmed.substring(PERSISTENT_MAC.length))
                    }
                    result.add(parser.line)
                }
            }
            if (target == null && !RepeaterService.persistentSupported) {
                result.add("")
                result.add(NetworkBlock().apply {
                    // generate a basic network block, it is likely that vendor is going to add more stuff here
                    add("network={")
                    ssidLine = size
                    add("")
                    bssidLine = size
                    bssid = bssids.singleOrNull()
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
                    target = this
                })
            }
            content = Content(result, target!!, persistentMacLine, legacy)
        } catch (e: Exception) {
            FirebaseCrashlytics.getInstance().apply {
                setCustomKey(TAG, config)
                setCustomKey("$TAG.ownerAddress", ownerAddress.toString())
                setCustomKey("$TAG.p2pGroup", group.toString())
            }
            throw e
        }
    }
    val psk by lazy { group?.passphrase ?: content.target.psk!! }
    val bssid by lazy {
        content.target.bssid?.let { MacAddress.fromString(it) }
    }

    suspend fun update(ssid: WifiSsidCompat, psk: String, bssid: MacAddress?) {
        val (lines, block, persistentMacLine, legacy) = content
        block[block.ssidLine!!] = "\tssid=${ssid.hex}"
        block[block.pskLine!!] = "\tpsk=\"$psk\""   // no control chars or weird stuff
        if (bssid != null) {
            persistentMacLine?.let { lines[it] = PERSISTENT_MAC + bssid }
            block[block.bssidLine!!] = "\tbssid=$bssid"
        }
        RootManager.use { it.execute(RepeaterCommands.WriteP2pConfig(lines.joinToString("\n"), legacy)) }
    }
}
