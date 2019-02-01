package be.mygod.vpnhotspot

import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.net.InterfaceAddress

class LocalOnlyInterfaceManager(val downstream: String) {
    private var routing: Routing? = null

    init {
        app.onPreCleanRoutings[this] = { routing?.stop() }
        app.onRoutingsCleaned[this] = this::clean
        initRouting()
    }

    private fun clean() {
        initRouting((routing ?: return).hostAddress)
    }

    private fun initRouting(owner: InterfaceAddress? = null) {
        routing = try {
            Routing(downstream, owner).apply {
                try {
                    ipForward() // local only interfaces need to enable ip_forward
                    forward()
                    masquerade(Routing.masquerade)
                    commit(true)
                } catch (e: Exception) {
                    revert()
                    throw e
                }   // otw nothing needs to be done
            }
        } catch (e: Exception) {
            SmartSnackbar.make(e).show()
            Timber.w(e)
            null
        }
    }

    fun stop() {
        app.onPreCleanRoutings -= this
        app.onRoutingsCleaned -= this
        routing?.revert()
    }
}
