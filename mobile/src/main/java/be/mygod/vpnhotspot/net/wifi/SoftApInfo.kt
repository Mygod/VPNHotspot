package be.mygod.vpnhotspot.net.wifi

import android.annotation.TargetApi
import android.net.MacAddress
import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.util.ConstantLookup
import be.mygod.vpnhotspot.util.UnblockCentral
import timber.log.Timber

@JvmInline
@RequiresApi(30)
value class SoftApInfo(val inner: Parcelable) {
    companion object {
        val clazz by lazy { Class.forName("android.net.wifi.SoftApInfo") }
        private val getFrequency by lazy { clazz.getDeclaredMethod("getFrequency") }
        private val getBandwidth by lazy { clazz.getDeclaredMethod("getBandwidth") }
        @get:RequiresApi(31)
        private val getBssid by lazy { clazz.getDeclaredMethod("getBssid") }
        @get:RequiresApi(31)
        private val getWifiStandard by lazy { clazz.getDeclaredMethod("getWifiStandard") }
        @get:RequiresApi(31)
        private val getApInstanceIdentifier by lazy @TargetApi(31) { UnblockCentral.getApInstanceIdentifier(clazz) }
        @get:RequiresApi(31)
        private val getAutoShutdownTimeoutMillis by lazy { clazz.getDeclaredMethod("getAutoShutdownTimeoutMillis") }

        val channelWidthLookup = ConstantLookup("CHANNEL_WIDTH_") { clazz }
    }

    val frequency get() = getFrequency(inner) as Int
    val bandwidth get() = getBandwidth(inner) as Int
    @get:RequiresApi(31)
    val bssid get() = getBssid(inner) as MacAddress?
    @get:RequiresApi(31)
    val wifiStandard get() = getWifiStandard(inner) as Int
    @get:RequiresApi(31)
    val apInstanceIdentifier get() = try {
        getApInstanceIdentifier(inner) as? String
    } catch (e: ReflectiveOperationException) {
        Timber.w(e)
        null
    }
    @get:RequiresApi(31)
    val autoShutdownTimeoutMillis get() = getAutoShutdownTimeoutMillis(inner) as Long
}
