package be.mygod.vpnhotspot.root

import be.mygod.librootkotlinx.ParcelableBoolean
import be.mygod.librootkotlinx.RootCommand
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import kotlinx.android.parcel.Parcelize

object WifiApCommands {
    @Parcelize
    class GetConfiguration : RootCommand<SoftApConfigurationCompat> {
        override suspend fun execute() = WifiApManager.configuration
    }

    @Parcelize
    data class SetConfiguration(val configuration: SoftApConfigurationCompat) : RootCommand<ParcelableBoolean> {
        override suspend fun execute() = ParcelableBoolean(WifiApManager.setConfiguration(configuration))
    }
}
