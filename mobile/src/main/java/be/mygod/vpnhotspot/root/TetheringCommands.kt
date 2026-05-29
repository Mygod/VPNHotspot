package be.mygod.vpnhotspot.root

import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.RootCommandNoResult
import be.mygod.librootkotlinx.RootFlow
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.util.Services
import kotlinx.coroutines.flow.filterIsInstance
import kotlinx.parcelize.Parcelize

object TetheringCommands {
    @Parcelize
    @RequiresApi(30)
    data class Start(private val type: Int, private val showProvisioningUi: Boolean) : RootCommandNoResult {
        override suspend fun execute() = null.also {
            TetheringManagerCompat.startTethering(type, true, showProvisioningUi)
        }
    }
    @Parcelize
    @RequiresApi(30)
    data class Stop(private val type: Int) : RootCommandNoResult {
        override suspend fun execute() = null.also { TetheringManagerCompat.stopTethering(type, Services.context) }
    }
    @Deprecated("Old API since API 30")
    @Parcelize
    @Suppress("DEPRECATION")
    data class StartLegacy(private val type: Int, private val showProvisioningUi: Boolean) : RootCommandNoResult {
        override suspend fun execute() = null.also {
            TetheringManagerCompat.startTetheringLegacy(type, showProvisioningUi)
        }
    }
    @Parcelize
    data class StopLegacy(private val type: Int) : RootCommandNoResult {
        override suspend fun execute() = null.also { TetheringManagerCompat.stopTetheringLegacy(type) }
    }

    /**
     * Tethered clients are the only tethering events forwarded over root, since the others don't
     * require signature permissions and are observed in-process via [TetheringManagerCompat.eventFlow].
     */
    @Parcelize
    @RequiresApi(30)
    class ClientsFlow : RootFlow<TetheringManagerCompat.Event.ClientsChanged> {
        override fun flow() =
            TetheringManagerCompat.eventFlow.filterIsInstance<TetheringManagerCompat.Event.ClientsChanged>()
    }
}
