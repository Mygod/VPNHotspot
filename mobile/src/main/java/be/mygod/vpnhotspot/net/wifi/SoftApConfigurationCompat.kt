package be.mygod.vpnhotspot.net.wifi

import android.annotation.TargetApi
import android.net.MacAddress
import android.net.wifi.SoftApConfiguration
import android.os.Build
import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.net.MacAddressCompat.Companion.toCompat
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import kotlinx.android.parcel.Parcelize

@Parcelize
data class SoftApConfigurationCompat(
        var ssid: String?,
        var securityType: Int,
        var passphrase: String?,
        @RequiresApi(23)
        var band: Int,
        @RequiresApi(23)
        var channel: Int,
        @Deprecated("Workaround for using inline class with Parcelize, use bssid")
        var bssidAddr: Long?,
        var maxNumberOfClients: Int,
        @RequiresApi(28)
        var shutdownTimeoutMillis: Long,
        @RequiresApi(28)
        var isAutoShutdownEnabled: Boolean,
        var isClientControlByUserEnabled: Boolean,
        var isHiddenSsid: Boolean,
        // TODO: WifiClient? nullable?
        var allowedClientList: List<Parcelable>?,
        var blockedClientList: List<Parcelable>?,
        val underlying: Parcelable? = null) : Parcelable {
    companion object {
        /**
         * TODO
         */
        const val BAND_ANY = -1
        const val BAND_2GHZ = 0
        const val BAND_5GHZ = 1
        const val BAND_6GHZ = 2
        const val CH_INVALID = 0

        // TODO: localize?
        val securityTypes = arrayOf("OPEN", "WPA2-PSK", "WPA3-SAE", "WPA3-SAE Transition mode")

        private val qrSanitizer = Regex("([\\\\\":;,])")

        /**
         * The frequency which AP resides on (MHz). Resides in range [2412, 5815].
         */
        fun channelToFrequency(channel: Int) = when (channel) {
            in 1..14 -> 2407 + 5 * channel
            in 15..165 -> 5000 + 5 * channel
            else -> throw IllegalArgumentException("Invalid channel $channel")
        }
        fun frequencyToChannel(frequency: Int) = when (frequency % 5) {
            2 -> ((frequency - 2407) / 5).also { check(it in 1..14) { "Invalid 2.4 GHz frequency $frequency" } }
            0 -> ((frequency - 5000) / 5).also { check(it in 15..165) { "Invalid 5 GHz frequency $frequency" } }
            else -> throw IllegalArgumentException("Invalid frequency $frequency")
        }

        /**
         * apBand and apChannel is available since API 23.
         *
         * https://android.googlesource.com/platform/frameworks/base/+/android-6.0.0_r1/wifi/java/android/net/wifi/WifiConfiguration.java#242
         */
        @get:RequiresApi(23)
        @Suppress("DEPRECATION")
        /**
         * The band which AP resides on
         * -1:Any 0:2G 1:5G
         * By default, 2G is chosen
         */
        private val apBand by lazy { android.net.wifi.WifiConfiguration::class.java.getDeclaredField("apBand") }
        @get:RequiresApi(23)
        @Suppress("DEPRECATION")
        /**
         * The channel which AP resides on
         * 2G  1-11
         * 5G  36,40,44,48,149,153,157,161,165
         * 0 - find a random available channel according to the apBand
         */
        private val apChannel by lazy {
            android.net.wifi.WifiConfiguration::class.java.getDeclaredField("apChannel")
        }

        @get:RequiresApi(30)
        private val getAllowedClientList by lazy @TargetApi(30) {
            SoftApConfiguration::class.java.getDeclaredMethod("getAllowedClientList")
        }
        @get:RequiresApi(30)
        private val getBand by lazy @TargetApi(30) { SoftApConfiguration::class.java.getDeclaredMethod("getBand") }
        @get:RequiresApi(30)
        private val getBlockedClientList by lazy @TargetApi(30) {
            SoftApConfiguration::class.java.getDeclaredMethod("getBlockedClientList")
        }
        @get:RequiresApi(30)
        private val getChannel by lazy @TargetApi(30) {
            SoftApConfiguration::class.java.getDeclaredMethod("getChannel")
        }
        @get:RequiresApi(30)
        private val getMaxNumberOfClients by lazy @TargetApi(30) {
            SoftApConfiguration::class.java.getDeclaredMethod("getMaxNumberOfClients")
        }
        @get:RequiresApi(30)
        private val getShutdownTimeoutMillis by lazy @TargetApi(30) {
            SoftApConfiguration::class.java.getDeclaredMethod("getShutdownTimeoutMillis")
        }
        @get:RequiresApi(30)
        private val isAutoShutdownEnabled by lazy @TargetApi(30) {
            SoftApConfiguration::class.java.getDeclaredMethod("isAutoShutdownEnabled")
        }
        @get:RequiresApi(30)
        private val isClientControlByUserEnabled by lazy @TargetApi(30) {
            SoftApConfiguration::class.java.getDeclaredMethod("isClientControlByUserEnabled")
        }

        @get:RequiresApi(30)
        private val classBuilder by lazy { Class.forName("android.net.wifi.SoftApConfiguration\$Builder") }
        @get:RequiresApi(30)
        private val newBuilder by lazy @TargetApi(30) { classBuilder.getConstructor(SoftApConfiguration::class.java) }
        @get:RequiresApi(30)
        private val build by lazy { classBuilder.getDeclaredMethod("build") }
        @get:RequiresApi(30)
        private val setAllowedClientList by lazy {
            classBuilder.getDeclaredMethod("setAllowedClientList", java.util.List::class.java)
        }
        @get:RequiresApi(30)
        private val setAutoShutdownEnabled by lazy {
            classBuilder.getDeclaredMethod("setAutoShutdownEnabled", Boolean::class.java)
        }
        @get:RequiresApi(30)
        private val setBand by lazy { classBuilder.getDeclaredMethod("setBand", Int::class.java) }
        @get:RequiresApi(30)
        private val setBlockedClientList by lazy {
            classBuilder.getDeclaredMethod("setBlockedClientList", java.util.List::class.java)
        }
        @get:RequiresApi(30)
        private val setBssid by lazy @TargetApi(30) {
            classBuilder.getDeclaredMethod("setBssid", MacAddress::class.java)
        }
        @get:RequiresApi(30)
        private val setChannel by lazy { classBuilder.getDeclaredMethod("setChannel", Int::class.java) }
        @get:RequiresApi(30)
        private val setClientControlByUserEnabled by lazy {
            classBuilder.getDeclaredMethod("setClientControlByUserEnabled", Boolean::class.java)
        }
        @get:RequiresApi(30)
        private val setHiddenSsid by lazy { classBuilder.getDeclaredMethod("setHiddenSsid", Boolean::class.java) }
        @get:RequiresApi(30)
        private val setMaxNumberOfClients by lazy {
            classBuilder.getDeclaredMethod("setMaxNumberOfClients", Int::class.java)
        }
        @get:RequiresApi(30)
        private val setPassphrase by lazy { classBuilder.getDeclaredMethod("setPassphrase", String::class.java) }
        @get:RequiresApi(30)
        private val setShutdownTimeoutMillis by lazy {
            classBuilder.getDeclaredMethod("setShutdownTimeoutMillis", Long::class.java)
        }
        @get:RequiresApi(30)
        private val setSsid by lazy { classBuilder.getDeclaredMethod("setSsid", String::class.java) }

        @Deprecated("Class deprecated in framework")
        @Suppress("DEPRECATION")
        fun android.net.wifi.WifiConfiguration.toCompat() = SoftApConfigurationCompat(
                SSID,
                allowedKeyManagement.nextSetBit(0).let { selected ->
                    require(allowedKeyManagement.nextSetBit(selected + 1) < 0) {
                        "More than 1 key managements supplied: $allowedKeyManagement"
                    }
                    when (if (selected < 0) -1 else selected) {
                        -1,     // getAuthType returns NONE if nothing is selected
                        android.net.wifi.WifiConfiguration.KeyMgmt.NONE -> SoftApConfiguration.SECURITY_TYPE_OPEN
                        android.net.wifi.WifiConfiguration.KeyMgmt.WPA_PSK,
                        4,      // WPA2_PSK
                        11 -> { // WPA_PSK_SHA256
                            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
                        }
                        android.net.wifi.WifiConfiguration.KeyMgmt.SAE -> SoftApConfiguration.SECURITY_TYPE_WPA3_SAE
                        // TODO: check source code
                        else -> throw IllegalArgumentException("Unrecognized key management: $allowedKeyManagement")
                    }
                },
                preSharedKey,
                if (Build.VERSION.SDK_INT >= 23) apBand.getInt(this) else BAND_ANY,         // TODO
                if (Build.VERSION.SDK_INT >= 23) apChannel.getInt(this) else CH_INVALID,    // TODO
                BSSID?.let { MacAddressCompat.fromString(it) }?.addr,
                0,  // TODO: unsupported field should have @RequiresApi?
                if (Build.VERSION.SDK_INT >= 28) {
                    TetherTimeoutMonitor.timeout.toLong()
                } else TetherTimeoutMonitor.MIN_SOFT_AP_TIMEOUT_DELAY_MS.toLong(),
                if (Build.VERSION.SDK_INT >= 28) TetherTimeoutMonitor.enabled else false,
                false,  // TODO
                hiddenSSID,
                null,
                null,
                this)

        @RequiresApi(30)
        @Suppress("UNCHECKED_CAST")
        fun SoftApConfiguration.toCompat() = SoftApConfigurationCompat(
                ssid,
                securityType,
                passphrase,
                getBand(this) as Int,
                getChannel(this) as Int,
                bssid?.toCompat()?.addr,
                getMaxNumberOfClients(this) as Int,
                getShutdownTimeoutMillis(this) as Long,
                isAutoShutdownEnabled(this) as Boolean,
                isClientControlByUserEnabled(this) as Boolean,
                isHiddenSsid,
                getAllowedClientList(this) as List<Parcelable>?,
                getBlockedClientList(this) as List<Parcelable>?,
                this)

        fun empty() = SoftApConfigurationCompat(
                null, SoftApConfiguration.SECURITY_TYPE_OPEN, null, BAND_ANY, CH_INVALID, null, 0,
                if (Build.VERSION.SDK_INT >= 28) {
                    TetherTimeoutMonitor.timeout.toLong()
                } else TetherTimeoutMonitor.MIN_SOFT_AP_TIMEOUT_DELAY_MS.toLong(),
                if (Build.VERSION.SDK_INT >= 28) TetherTimeoutMonitor.enabled else false, false, false, null, null)
    }

    @Suppress("DEPRECATION")
    inline var bssid: MacAddressCompat?
        get() = bssidAddr?.let { MacAddressCompat(it) }
        set(value) {
            bssidAddr = value?.addr
        }

    /**
     * Based on:
     * https://android.googlesource.com/platform/packages/apps/Settings/+/android-5.0.0_r1/src/com/android/settings/wifi/WifiApDialog.java#88
     * https://android.googlesource.com/platform/packages/apps/Settings/+/b1af85d/src/com/android/settings/wifi/tether/WifiTetherSettings.java#162
     */
    @Deprecated("Class deprecated in framework")
    @Suppress("DEPRECATION")
    fun toWifiConfiguration(): android.net.wifi.WifiConfiguration {
        val wc = underlying as? android.net.wifi.WifiConfiguration
        val result = if (wc == null) android.net.wifi.WifiConfiguration() else android.net.wifi.WifiConfiguration(wc)
        val original = wc?.toCompat()
        result.SSID = ssid
        if (original?.securityType != securityType) {
            result.allowedKeyManagement.clear()
            result.allowedKeyManagement.set(when (securityType) {
                SoftApConfiguration.SECURITY_TYPE_OPEN -> android.net.wifi.WifiConfiguration.KeyMgmt.NONE
                // not actually used on API 30-
                SoftApConfiguration.SECURITY_TYPE_WPA2_PSK -> android.net.wifi.WifiConfiguration.KeyMgmt.WPA_PSK
                SoftApConfiguration.SECURITY_TYPE_WPA3_SAE,
                SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION -> android.net.wifi.WifiConfiguration.KeyMgmt.SAE
                else -> throw IllegalArgumentException("Unsupported securityType $securityType")
            })
            result.allowedAuthAlgorithms.clear()
            result.allowedAuthAlgorithms.set(android.net.wifi.WifiConfiguration.AuthAlgorithm.OPEN)
        }
        result.preSharedKey = passphrase
        if (Build.VERSION.SDK_INT >= 23) {
            apBand.setInt(result, band)
            apChannel.setInt(result, channel)
        }
        if (bssid != original?.bssid) result.BSSID = bssid?.toString()
        result.hiddenSSID = isHiddenSsid
        return result
    }

    @RequiresApi(30)
    fun toPlatform(): SoftApConfiguration {
        val sac = underlying as? SoftApConfiguration
        // TODO: can we always call copy constructor?
        val builder = if (sac == null) classBuilder.newInstance() else newBuilder.newInstance(sac)
        setSsid(builder, ssid)
        // TODO: setSecurityType
        setPassphrase(builder, passphrase)
        setBand(builder, band)
        setChannel(builder, channel)
        setBssid(builder, bssid?.toPlatform())
        setMaxNumberOfClients(builder, maxNumberOfClients)
        setShutdownTimeoutMillis(builder, shutdownTimeoutMillis)
        setAutoShutdownEnabled(builder, isAutoShutdownEnabled)
        setClientControlByUserEnabled(builder, isClientControlByUserEnabled)
        setHiddenSsid(builder, isHiddenSsid)
        setAllowedClientList(builder, allowedClientList)
        setBlockedClientList(builder, blockedClientList)
        return build(builder) as SoftApConfiguration
    }

    /**
     * Documentation: https://github.com/zxing/zxing/wiki/Barcode-Contents#wi-fi-network-config-android-ios-11
     * Based on: https://android.googlesource.com/platform/packages/apps/Settings/+/4a5ff58/src/com/android/settings/wifi/dpp/WifiNetworkConfig.java#161
     */
    fun toQrCode() = StringBuilder("WIFI:").apply {
        fun String.sanitize() = qrSanitizer.replace(this) { "\\${it.groupValues[1]}" }
        when (securityType) {
            SoftApConfiguration.SECURITY_TYPE_OPEN -> { }
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK -> append("T:WPA;")
            SoftApConfiguration.SECURITY_TYPE_WPA3_SAE, SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION -> {
                append("T:SAE;")
            }
            else -> throw IllegalArgumentException("Unsupported authentication type")
        }
        append("S:")
        append(ssid!!.sanitize())
        append(';')
        passphrase?.let { passphrase ->
            append("P:")
            append(passphrase.sanitize())
            append(';')
        }
        if (isHiddenSsid) append("H:true;")
        append(';')
    }.toString()
}
