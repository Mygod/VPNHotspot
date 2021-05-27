package be.mygod.vpnhotspot.net.wifi

import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.util.LongConstantLookup

@JvmInline
@RequiresApi(30)
value class SoftApCapability(val inner: Parcelable) {
    companion object {
        private val clazz by lazy { Class.forName("android.net.wifi.SoftApCapability") }
        private val getMaxSupportedClients by lazy { clazz.getDeclaredMethod("getMaxSupportedClients") }
        private val areFeaturesSupported by lazy { clazz.getDeclaredMethod("areFeaturesSupported", Long::class.java) }
        val featureLookup by lazy { LongConstantLookup(clazz, "SOFTAP_FEATURE_") }
    }

    val maxSupportedClients get() = getMaxSupportedClients.invoke(inner) as Int
    val supportedFeatures: Long get() {
        var supportedFeatures = 0L
        var probe = 1L
        while (probe != 0L) {
            if (areFeaturesSupported(inner, probe) as Boolean) supportedFeatures = supportedFeatures or probe
            probe += probe
        }
        return supportedFeatures
    }
}
