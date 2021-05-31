package be.mygod.vpnhotspot.root

import android.net.wifi.SoftApConfiguration
import android.os.Parcelable
import androidx.annotation.RequiresApi
import kotlinx.parcelize.Parcelize

@RequiresApi(30)
sealed class LocalOnlyHotspotCallbacks : Parcelable {
    @Parcelize
    data class OnStarted(val config: SoftApConfiguration) : LocalOnlyHotspotCallbacks()
    @Parcelize
    class OnStopped : LocalOnlyHotspotCallbacks() {
        override fun equals(other: Any?) = other is OnStopped
        override fun hashCode() = 0x80acd3ca.toInt()
    }
    @Parcelize
    data class OnFailed(val reason: Int) : LocalOnlyHotspotCallbacks()
}
