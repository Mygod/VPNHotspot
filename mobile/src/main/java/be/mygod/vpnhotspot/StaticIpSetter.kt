package be.mygod.vpnhotspot

import android.annotation.SuppressLint
import android.content.Context
import android.net.LinkAddress
import androidx.core.content.edit
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.lang.reflect.InvocationTargetException
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketException
import java.security.SecureRandom

@Parcelize
class StaticIpSetter : BootReceiver.Startable {
    companion object {
        private const val KEY = "service.staticIp"

        val iface get() = try {
            NetworkInterface.getByName("lo")
        } catch (_: SocketException) {
            null
        } catch (e: Exception) {
            Timber.w(e)
            null
        }

        private val currentActive get() = iface?.interfaceAddresses?.any { !it.address.isLoopbackAddress } == true
        private val currentAddresses get() = buildList {
            for (address in iface?.interfaceAddresses.orEmpty()) if (!address.address.isLoopbackAddress) {
                add(address.address to address.networkPrefixLength)
            }
        }

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

        private val activeState = MutableStateFlow(currentActive)
        val active = activeState.asStateFlow()
        private val addressesState = MutableStateFlow(currentAddresses)
        val addresses = addressesState.asStateFlow()
        private val applyingState = MutableStateFlow(false)
        val applying = applyingState.asStateFlow()
        private fun refreshInterfaceState() {
            activeState.value = currentActive
            addressesState.value = currentAddresses
        }
        private var pendingEnabled: Boolean? = null

        @get:SuppressLint("SoonBlockedPrivateApi")
        private val constructorLinkAddress by lazy {
            LinkAddress::class.java.getDeclaredConstructor(String::class.java)
        }
        fun parseAddresses(value: String) = value.lineSequence().mapNotNull { line ->
            val trimmed = line.trim()
            if (trimmed.isBlank()) return@mapNotNull null
            constructorLinkAddress.newInstance(if ('/' in trimmed) trimmed else {
                "$trimmed/${if (':' in trimmed) 128 else 32}"
            })
        }

        fun enable(enabled: Boolean) {
            GlobalScope.launch(Dispatchers.Main.immediate) {
                pendingEnabled = enabled
                if (applying.value) {
                    refreshInterfaceState()
                    return@launch
                }
                applyingState.value = true
                refreshInterfaceState()
                try {
                    while (true) {
                        val next = pendingEnabled ?: break
                        pendingEnabled = null
                        val success = try {
                            DaemonController.replaceStaticAddresses("lo", if (next) {
                                parseAddresses(ips).asIterable()
                            } else emptyList())
                            next
                        } catch (e: CancellationException) {
                            throw e
                        } catch (e: IllegalArgumentException) {
                            SmartSnackbar.make(e).show()
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
                        refreshInterfaceState()
                    }
                } finally {
                    applyingState.value = false
                    refreshInterfaceState()
                }
            }
        }
    }

    override fun start(context: Context) {
        enable(true)
    }
}
