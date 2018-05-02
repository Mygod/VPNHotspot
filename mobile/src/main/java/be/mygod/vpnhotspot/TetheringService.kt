package be.mygod.vpnhotspot

import android.content.Intent
import android.os.Binder
import android.os.Bundle
import android.widget.Toast
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetheringManager

class TetheringService : BaseTetheringService() {
    companion object {
        const val EXTRA_ADD_INTERFACE = "interface.add"
        const val EXTRA_REMOVE_INTERFACE = "interface.remove"
    }

    inner class TetheringBinder : Binder() {
        var fragment: TetheringFragment? = null

        fun isActive(iface: String): Boolean = synchronized(routings) { routings.keys.contains(iface) }
    }

    private val binder = TetheringBinder()

    override fun onTetherStateChangedLocked(extras: Bundle) =
            removeRoutingsLocked(routings.keys - TetheringManager.getTetheredIfaces(extras))

    override fun updateRoutingsLocked() {
        super.updateRoutingsLocked()
        app.handler.post { binder.fragment?.adapter?.notifyDataSetChanged() }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent, flags: Int, startId: Int): Int {
        val iface = intent.getStringExtra(EXTRA_ADD_INTERFACE)
        synchronized(routings) {
            if (iface != null) routings[iface] = null
            if (routings.remove(intent.getStringExtra(EXTRA_REMOVE_INTERFACE))?.stop() == false)
                Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
            updateRoutingsLocked()
        }
        return START_NOT_STICKY
    }
}
