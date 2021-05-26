package be.mygod.vpnhotspot.net.wifi

import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.util.ConstantLookup

@JvmInline
@RequiresApi(30)
value class SoftApInfo(val inner: Parcelable) {
    companion object {
        private val classSoftApInfo by lazy { Class.forName("android.net.wifi.SoftApInfo") }
        private val getFrequency by lazy { classSoftApInfo.getDeclaredMethod("getFrequency") }
        private val getBandwidth by lazy { classSoftApInfo.getDeclaredMethod("getBandwidth") }

        val channelWidthLookup = ConstantLookup("CHANNEL_WIDTH_") { classSoftApInfo }
    }

    val frequency get() = getFrequency.invoke(inner) as Int
    val bandwidth get() = getBandwidth.invoke(inner) as Int
}
