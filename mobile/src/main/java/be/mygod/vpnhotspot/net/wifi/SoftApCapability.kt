package be.mygod.vpnhotspot.net.wifi

import android.annotation.TargetApi
import android.os.Build
import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.util.LongConstantLookup
import be.mygod.vpnhotspot.util.UnblockCentral
import timber.log.Timber

@JvmInline
@RequiresApi(30)
value class SoftApCapability(val inner: Parcelable) {
    companion object {
        val clazz by lazy { Class.forName("android.net.wifi.SoftApCapability") }
        private val getMaxSupportedClients by lazy { clazz.getDeclaredMethod("getMaxSupportedClients") }
        private val areFeaturesSupported by lazy { clazz.getDeclaredMethod("areFeaturesSupported", Long::class.java) }
        @get:RequiresApi(31)
        private val getSupportedChannelList by lazy {
            clazz.getDeclaredMethod("getSupportedChannelList", Int::class.java)
        }
        @get:RequiresApi(31)
        @get:TargetApi(33)
        private val getCountryCode by lazy { UnblockCentral.getCountryCode(clazz) }

        @RequiresApi(31)
        const val SOFTAP_FEATURE_BAND_24G_SUPPORTED = 32L
        @RequiresApi(31)
        const val SOFTAP_FEATURE_BAND_5G_SUPPORTED = 64L
        @RequiresApi(31)
        const val SOFTAP_FEATURE_BAND_6G_SUPPORTED = 128L
        @RequiresApi(31)
        const val SOFTAP_FEATURE_BAND_60G_SUPPORTED = 256L
        val featureLookup by lazy { LongConstantLookup(clazz, "SOFTAP_FEATURE_") }
    }

    val maxSupportedClients get() = getMaxSupportedClients(inner) as Int
    val supportedFeatures: Long get() {
        var supportedFeatures = 0L
        var probe = 1L
        while (probe != 0L) {
            if (areFeaturesSupported(inner, probe) as Boolean) supportedFeatures = supportedFeatures or probe
            probe += probe
        }
        return supportedFeatures
    }
    fun getSupportedChannelList(band: Int) = getSupportedChannelList(inner, band) as IntArray
    @get:RequiresApi(31)
    val countryCode: String? get() = try {
        getCountryCode(inner) as String?
    } catch (e: ReflectiveOperationException) {
        if (Build.VERSION.SDK_INT >= 33) Timber.w(e)
        null
    }
}
