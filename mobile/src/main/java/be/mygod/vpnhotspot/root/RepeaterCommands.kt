package be.mygod.vpnhotspot.root

import android.net.MacAddress
import android.net.wifi.ScanResult
import android.net.wifi.p2p.WifiP2pManager
import android.os.Looper
import android.os.Parcelable
import android.system.Os
import android.system.OsConstants
import android.text.TextUtils
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.*
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.deletePersistentGroup
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestDeviceAddress
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestPersistentGroupInfo
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.setVendorElements
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.setWifiP2pChannels
import be.mygod.vpnhotspot.util.Services
import kotlinx.parcelize.Parcelize
import java.io.File
import java.io.IOException

object RepeaterCommands {
    @Parcelize
    class Deinit : RootCommandNoResult {
        override suspend fun execute(): Parcelable? {
            channel = null
            return null
        }
    }

    @Parcelize
    data class DeletePersistentGroup(val netId: Int) : RootCommand<ParcelableInt?> {
        override suspend fun execute() = Services.p2p!!.run {
            deletePersistentGroup(obtainChannel(), netId)?.let { ParcelableInt(it) }
        }
    }

    @Parcelize
    @RequiresApi(29)
    class RequestDeviceAddress : RootCommand<MacAddress?> {
        override suspend fun execute() = Services.p2p!!.run { requestDeviceAddress(obtainChannel()) }
    }

    @Parcelize
    class RequestPersistentGroupInfo : RootCommand<ParcelableList> {
        override suspend fun execute() = Services.p2p!!.run {
            ParcelableList(requestPersistentGroupInfo(obtainChannel()).toList())
        }
    }

    @Parcelize
    data class SetChannel(private val oc: Int) : RootCommand<ParcelableInt?> {
        override suspend fun execute() = Services.p2p!!.run {
            setWifiP2pChannels(obtainChannel(), 0, oc)?.let { ParcelableInt(it) }
        }
    }

    @Parcelize
    @RequiresApi(33)
    data class SetVendorElements(private val ve: List<ScanResult.InformationElement>) : RootCommand<ParcelableInt?> {
        override suspend fun execute() = Services.p2p!!.run {
            setVendorElements(obtainChannel(), ve)?.let { ParcelableInt(it) }
        }
    }

    @Parcelize
    data class WriteP2pConfig(val data: String, val legacy: Boolean) : RootCommandNoResult {
        override suspend fun execute(): Parcelable? {
            File(if (legacy) CONF_PATH_LEGACY else CONF_PATH_TREBLE).writeText(data)
            for (process in File("/proc").listFiles { _, name -> TextUtils.isDigitsOnly(name) }!!) {
                val cmdline = try {
                    File(process, "cmdline").inputStream().bufferedReader().use { it.readText() }
                } catch (_: IOException) {
                    continue
                }
                if (File(cmdline.split(Char.MIN_VALUE, limit = 2).first()).name == "wpa_supplicant") {
                    Os.kill(process.name.toInt(), OsConstants.SIGTERM)
                }
            }
            return null
        }
    }

    @Parcelize
    class ReadP2pConfig : RootCommand<WriteP2pConfig> {
        private fun test(path: String) = File(path).run {
            if (canRead()) readText() else null
        }

        override suspend fun execute(): WriteP2pConfig {
            test(CONF_PATH_TREBLE)?.let { return WriteP2pConfig(it, false) }
            test(CONF_PATH_LEGACY)?.let { return WriteP2pConfig(it, true) }
            error("p2p config file not found")
        }
    }

    private const val CONF_PATH_TREBLE = "/data/vendor/wifi/wpa/p2p_supplicant.conf"
    private const val CONF_PATH_LEGACY = "/data/misc/wifi/p2p_supplicant.conf"
    private var channel: WifiP2pManager.Channel? = null

    private fun WifiP2pManager.obtainChannel(): WifiP2pManager.Channel {
        channel?.let { return it }
        val uninitializer = object : WifiP2pManager.ChannelListener {
            var target: WifiP2pManager.Channel? = null
            override fun onChannelDisconnected() {
                if (target == channel) channel = null
            }
        }
        return initialize(systemContext, Looper.getMainLooper(), uninitializer).also {
            uninitializer.target = it
            channel = it    // cache the instance until invalidated
        }
    }
}
