package be.mygod.vpnhotspot

import android.content.Context
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.net.InetAddress
import java.net.InterfaceAddress

class LocalOnlyInterfaceManager(private val owner: Context, val downstream: String) : UpstreamMonitor.Callback {
    private var routing: Routing? = null
    private var dns = emptyList<InetAddress>()

    init {
        app.cleanRoutings[this] = this::clean
        UpstreamMonitor.registerCallback(this) { initRouting() }
    }

    override fun onAvailable(ifname: String, dns: List<InetAddress>) {
        val routing = routing
        initRouting(ifname, if (routing == null) null else {
            routing.revert()
            routing.hostAddress
        }, dns)
    }
    override fun onLost() {
        val routing = routing ?: return
        routing.revert()
        initRouting(null, routing.hostAddress, emptyList())
    }

    private fun clean() {
        val routing = routing ?: return
        routing.stop()
        initRouting(routing.upstream, routing.hostAddress, dns)
    }

    private fun initRouting(upstream: String? = null, owner: InterfaceAddress? = null,
                            dns: List<InetAddress> = this.dns) {
        this.dns = dns
        try {
            routing = Routing(this.owner, upstream, downstream, owner, app.strict).apply {
                try {
                    if (app.dhcpWorkaround) dhcpWorkaround()
                    ipForward()                                     // local only interfaces need to enable ip_forward
                    rule()
                    forward()
                    if (app.masquerade) masquerade()
                    dnsRedirect(dns)
                    commit()
                } catch (e: Exception) {
                    revert()
                    throw e
                }   // otw nothing needs to be done
            }
        } catch (e: Exception) {
            SmartSnackbar.make(e.localizedMessage).show()
            Timber.w(e)
            routing = null
        }
    }

    fun stop() {
        UpstreamMonitor.unregisterCallback(this)
        app.cleanRoutings -= this
        routing?.revert()
    }
}
