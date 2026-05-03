package be.mygod.vpnhotspot

import android.content.Context
import android.text.SpannableStringBuilder
import androidx.core.content.edit
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.root.RoutingCommands
import be.mygod.vpnhotspot.util.Event0
import be.mygod.vpnhotspot.util.makeIpSpan
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.io.IOException
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketException
import java.security.SecureRandom

@Parcelize
class StaticIpSetter : BootReceiver.Startable {
    companion object {
        private const val KEY = "service.staticIp"

        val ifaceEvent = Event0()

        val iface get() = try {
            NetworkInterface.getByName("lo")
        } catch (_: SocketException) {
            null
        } catch (e: Exception) {
            Timber.w(e)
            null
        }

        val active get() = iface?.interfaceAddresses?.any { !it.address.isLoopbackAddress } == true
        val addresses get() = SpannableStringBuilder().apply {
            for (address in iface?.interfaceAddresses.orEmpty()) if (!address.address.isLoopbackAddress) {
                append(makeIpSpan(address.address))
                address.networkPrefixLength.also {
                    if (it.toInt() != address.address.address.size * 8) append("/$it")
                }
                appendLine()
            }
        }.trimEnd()

        var ips: String
            get() {
                app.pref.getString(KEY, null).let { if (!it.isNullOrBlank()) return it }
                val random = SecureRandom.getInstanceStrong()
                val octets = ByteArray(3)
                val globalId = ByteArray(5)
                random.nextBytes(octets)
                random.nextBytes(globalId)
                return "10.${octets.joinToString(".") { it.toUByte().toString() }}\n${
                    InetAddress.getByAddress(ByteArray(16).apply {
                        this[0] = 0xfd.toByte()
                        globalId.copyInto(this, 1)
                        this[15] = 1
                    }).hostAddress
                }".also { ips = it }
            }
            set(value) = app.pref.edit { putString(KEY, value) }

        fun enable(enabled: Boolean) = GlobalScope.launch {
            val success = try {
                RootSession.use {
                    try {
                        if (enabled) {
                            ips.lineSequence().forEach { ip -> it.exec("${Routing.IP} addr replace $ip dev lo") }
                            true
                        } else {
                            val addresses = iface?.interfaceAddresses
                            if (addresses != null) for (address in addresses) if (!address.address.isLoopbackAddress) {
                                it.exec("${Routing.IP} addr del ${address.address.hostAddress}/${
                                    address.networkPrefixLength} dev lo")
                            }
                            false
                        }
                    } catch (e: RoutingCommands.UnexpectedOutputException) {
                        if (Routing.shouldSuppressIpError(e, enabled)) return@use null
                        Timber.w(IOException("Failed to modify link", e))
                        SmartSnackbar.make(e).show()
                        null
                    }
                }
            } catch (_: CancellationException) {
                null
            } catch (e: Exception) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
                null
            }
            when (success) {
                true -> BootReceiver.add<StaticIpSetter>(StaticIpSetter())
                false -> BootReceiver.delete<StaticIpSetter>()
                null -> { }
            }
            ifaceEvent()
        }
    }

    override fun start(context: Context) {
        enable(true)
    }
}
