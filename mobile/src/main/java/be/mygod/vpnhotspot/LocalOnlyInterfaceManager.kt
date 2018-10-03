package be.mygod.vpnhotspot

import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.net.InterfaceAddress

class LocalOnlyInterfaceManager(val downstream: String) {
    private var routing: Routing? = null

    init {
        app.cleanRoutings[this] = this::clean
        initRouting()
    }

    private fun clean() {
        val routing = routing ?: return
        routing.stop()
        initRouting(routing.hostAddress)
    }

    private fun initRouting(owner: InterfaceAddress? = null) {
        routing = try {
            Routing(downstream, owner).apply {
                try {
                    if (app.dhcpWorkaround) dhcpWorkaround()
                    ipForward()                                     // local only interfaces need to enable ip_forward
                    forward()
                    if (app.masquerade) masquerade()
                    commit()
                } catch (e: Exception) {
                    revert()
                    throw e
                }   // otw nothing needs to be done
            }
        } catch (e: Exception) {
            SmartSnackbar.make(e.localizedMessage).show()
            Timber.w(e)
            null
        }
    }

    fun stop() {
        app.cleanRoutings -= this
        routing?.revert()
    }
}
