package be.mygod.vpnhotspot.net.wifi

import android.net.wifi.WifiClient
import android.os.Build
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.util.UnblockCentral
import timber.log.Timber

@get:RequiresApi(31)
val WifiClient.apInstanceIdentifierOrNull: String? get() = try {
    UnblockCentral.getApInstanceIdentifier(this)
} catch (e: NoSuchMethodError) {
    if (Build.VERSION.SDK_INT >= 31) Timber.w(e)
    null
}
