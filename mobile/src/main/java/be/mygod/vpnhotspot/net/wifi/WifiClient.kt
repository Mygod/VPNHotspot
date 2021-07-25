package be.mygod.vpnhotspot.net.wifi

import android.annotation.TargetApi
import android.net.MacAddress
import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.util.UnblockCentral
import timber.log.Timber

@JvmInline
@RequiresApi(30)
value class WifiClient(val inner: Parcelable) {
    companion object {
        val clazz by lazy { Class.forName("android.net.wifi.WifiClient") }
        private val getMacAddress by lazy { clazz.getDeclaredMethod("getMacAddress") }
        @get:RequiresApi(31)
        private val getApInstanceIdentifier by lazy @TargetApi(31) { UnblockCentral.getApInstanceIdentifier(clazz) }
    }

    val macAddress get() = getMacAddress(inner) as MacAddress
    @get:RequiresApi(31)
    val apInstanceIdentifier get() = try {
        getApInstanceIdentifier(inner) as? String
    } catch (e: ReflectiveOperationException) {
        Timber.w(e)
        null
    }
}
