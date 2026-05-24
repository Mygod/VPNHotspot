package be.mygod.vpnhotspot.net.wifi

import android.net.wifi.SoftApInfo
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.util.ConstantLookup
import be.mygod.vpnhotspot.util.UnblockCentral
import timber.log.Timber

@RequiresApi(30)
val softApChannelWidthLookup = ConstantLookup("CHANNEL_WIDTH_") { SoftApInfo::class.java }

@get:RequiresApi(31)
val SoftApInfo.apInstanceIdentifierOrNull: String? get() = try {
    UnblockCentral.getApInstanceIdentifier(this)
} catch (e: NoSuchMethodError) {
    Timber.w(e)
    null
}
