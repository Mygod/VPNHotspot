package be.mygod.vpnhotspot

import android.content.Context
import androidx.core.content.edit
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.root.RoutingCommands
import be.mygod.vpnhotspot.util.Event0
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.io.IOException
import java.net.NetworkInterface
import java.net.SocketException
import java.security.SecureRandom

@Parcelize
class StaticIpSetter : BootReceiver.Startable {
    companion object {
        private const val IFACE = "staticip"
        private const val KEY = "service.staticIp"

        val ifaceEvent = Event0()

        val iface get() = try {
            NetworkInterface.getByName(IFACE)
        } catch (_: SocketException) {
            null
        } catch (e: Exception) {
            Timber.w(e)
            null
        }

        var ips: String
            get() {
                app.pref.getString(KEY, null)?.let { return it }
                val octets = ByteArray(3)
                SecureRandom.getInstanceStrong().nextBytes(octets)
                return "10.${octets.joinToString(".") { it.toUByte().toString() }}".also { ips = it }
            }
            set(value) = app.pref.edit { putString(KEY, value) }

        fun enable(enabled: Boolean) = GlobalScope.launch {
            val success = try {
                RootSession.use {
                    try {
                        if (enabled) {
                            it.exec("${Routing.IP} link add $IFACE type dummy")
                            ips.lineSequence().forEach { ip ->
                                it.exec("${Routing.IP} addr add $ip dev $IFACE")
                            }
                            it.exec("${Routing.IP} link set $IFACE up")
                            true
                        } else {
                            it.exec("${Routing.IP} link del $IFACE")
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
