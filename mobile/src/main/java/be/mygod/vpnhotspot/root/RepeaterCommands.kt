package be.mygod.vpnhotspot.root

import android.net.wifi.p2p.WifiP2pManager
import android.os.Looper
import android.os.Parcelable
import android.system.Os
import android.system.OsConstants
import android.text.TextUtils
import be.mygod.librootkotlinx.ParcelableInt
import be.mygod.librootkotlinx.RootCommand
import be.mygod.librootkotlinx.RootCommandNoResult
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.setWifiP2pChannels
import be.mygod.vpnhotspot.util.Services
import eu.chainfire.librootjava.RootJava
import kotlinx.android.parcel.Parcelize
import kotlinx.coroutines.CompletableDeferred
import java.io.File

object RepeaterCommands {
    @Parcelize
    class SetChannel(private val oc: Int, private val forceReinit: Boolean = false) : RootCommand<ParcelableInt?> {
        override suspend fun execute() = Services.p2p!!.run {
            if (forceReinit) channel = null
            val uninitializer = object : WifiP2pManager.ChannelListener {
                var target: WifiP2pManager.Channel? = null
                override fun onChannelDisconnected() {
                    if (target == channel) channel = null
                }
            }
            val channel = channel ?: initialize(RootJava.getSystemContext(),
                    Looper.getMainLooper(), uninitializer)
            uninitializer.target = channel
            RepeaterCommands.channel = channel  // cache the instance until invalidated
            val future = CompletableDeferred<Int?>()
            setWifiP2pChannels(channel, 0, oc, object : WifiP2pManager.ActionListener {
                override fun onSuccess() {
                    future.complete(null)
                }

                override fun onFailure(reason: Int) {
                    future.complete(reason)
                }
            })
            future.await()?.let { ParcelableInt(it) }
        }
    }

    @Parcelize
    data class WriteP2pConfig(val data: String, val legacy: Boolean) : RootCommandNoResult {
        override suspend fun execute(): Parcelable? {
            File(if (legacy) CONF_PATH_LEGACY else CONF_PATH_TREBLE).writeText(data)
            for (process in File("/proc").listFiles { _, name -> TextUtils.isDigitsOnly(name) }!!) {
                if (File(File(process, "cmdline").inputStream().bufferedReader().readText()
                                .split(Char.MIN_VALUE, limit = 2).first()).name == "wpa_supplicant") {
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
            throw IllegalStateException("p2p config file not found")
        }
    }

    private const val CONF_PATH_TREBLE = "/data/vendor/wifi/wpa/p2p_supplicant.conf"
    private const val CONF_PATH_LEGACY = "/data/misc/wifi/p2p_supplicant.conf"
    private var channel: WifiP2pManager.Channel? = null
}
