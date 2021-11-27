package be.mygod.vpnhotspot.net.wifi

import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.net.MacAddress
import android.net.wifi.SoftApConfiguration
import android.os.Build
import android.os.Parcelable
import android.util.SparseIntArray
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.net.MacAddressCompat.Companion.toCompat
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.util.ConstantLookup
import be.mygod.vpnhotspot.util.UnblockCentral
import kotlinx.parcelize.Parcelize
import timber.log.Timber

@Parcelize
data class SoftApConfigurationCompat(
        var ssid: String? = null,
        @Deprecated("Workaround for using inline class with Parcelize, use bssid")
        var bssidAddr: Long? = null,
        var passphrase: String? = null,
        var isHiddenSsid: Boolean = false,
        /**
         * You should probably set or modify this field directly only when you want to use bridged AP,
         * see also [android.net.wifi.WifiManager.isBridgedApConcurrencySupported].
         * Otherwise, use [requireSingleBand] and [setChannel].
         */
        @TargetApi(23)
        var channels: SparseIntArray = SparseIntArray(1).apply { append(BAND_2GHZ, 0) },
        var securityType: Int = SoftApConfiguration.SECURITY_TYPE_OPEN,
        @TargetApi(30)
        var maxNumberOfClients: Int = 0,
        @TargetApi(28)
        var isAutoShutdownEnabled: Boolean = true,
        @TargetApi(28)
        var shutdownTimeoutMillis: Long = 0,
        @TargetApi(30)
        var isClientControlByUserEnabled: Boolean = false,
        @RequiresApi(30)
        var blockedClientList: List<MacAddress> = emptyList(),
        @RequiresApi(30)
        var allowedClientList: List<MacAddress> = emptyList(),
        @TargetApi(31)
        var macRandomizationSetting: Int = RANDOMIZATION_PERSISTENT,
        @TargetApi(31)
        var isBridgedModeOpportunisticShutdownEnabled: Boolean = true,
        @TargetApi(31)
        var isIeee80211axEnabled: Boolean = true,
        @TargetApi(31)
        var isUserConfiguration: Boolean = true,
        var underlying: Parcelable? = null) : Parcelable {
    companion object {
        const val BAND_2GHZ = 1
        const val BAND_5GHZ = 2
        @TargetApi(30)
        const val BAND_6GHZ = 4
        @TargetApi(31)
        const val BAND_60GHZ = 8
        const val BAND_LEGACY = BAND_2GHZ or BAND_5GHZ
        @TargetApi(30)
        const val BAND_ANY_30 = BAND_LEGACY or BAND_6GHZ
        @TargetApi(31)
        const val BAND_ANY_31 = BAND_ANY_30 or BAND_60GHZ
        val BAND_TYPES by lazy {
            if (Build.VERSION.SDK_INT >= 31) try {
                return@lazy UnblockCentral.SoftApConfiguration_BAND_TYPES
            } catch (e: ReflectiveOperationException) {
                Timber.w(e)
            }
            intArrayOf(BAND_2GHZ, BAND_5GHZ, BAND_6GHZ, BAND_60GHZ)
        }
        @RequiresApi(31)
        val bandLookup = ConstantLookup<SoftApConfiguration>("BAND_")

        @TargetApi(31)
        const val RANDOMIZATION_NONE = 0
        @TargetApi(31)
        const val RANDOMIZATION_PERSISTENT = 1

        fun isLegacyEitherBand(band: Int) = band and BAND_LEGACY == BAND_LEGACY

        /**
         * [android.net.wifi.WifiConfiguration.KeyMgmt.WPA2_PSK]
         */
        private const val LEGACY_WPA2_PSK = 4

        val securityTypes = arrayOf("OPEN", "WPA2-PSK", "WPA3-SAE Transition mode", "WPA3-SAE")

        private val qrSanitizer = Regex("([\\\\\":;,])")

        /**
         * Based on:
         * https://elixir.bootlin.com/linux/v5.12.8/source/net/wireless/util.c#L75
         * https://cs.android.com/android/platform/superproject/+/master:packages/modules/Wifi/framework/java/android/net/wifi/ScanResult.java;l=789;drc=71d758698c45984d3f8de981bf98e56902480f16
         */
        fun channelToFrequency(band: Int, chan: Int) = when (band) {
            BAND_2GHZ -> when (chan) {
                14 -> 2484
                in 1 until 14 -> 2407 + chan * 5
                else -> throw IllegalArgumentException("Invalid 2GHz channel $chan")
            }
            BAND_5GHZ -> when (chan) {
                in 182..196 -> 4000 + chan * 5
                in 1..Int.MAX_VALUE -> 5000 + chan * 5
                else -> throw IllegalArgumentException("Invalid 5GHz channel $chan")
            }
            BAND_6GHZ -> when (chan) {
                2 -> 5935
                in 1..233 -> 5950 + chan * 5
                else -> throw IllegalArgumentException("Invalid 6GHz channel $chan")
            }
            BAND_60GHZ -> {
                require(chan in 1 until 7) { "Invalid 60GHz channel $chan" }
                56160 + chan * 2160
            }
            else -> throw IllegalArgumentException("Invalid band $band")
        }
        fun frequencyToChannel(freq: Int) = when (freq) {
            2484 -> 14
            in Int.MIN_VALUE until 2484 -> (freq - 2407) / 5
            in 4910..4980 -> (freq - 4000) / 5
            in Int.MIN_VALUE until 5925 -> (freq - 5000) / 5
            5935 -> 2
            in Int.MIN_VALUE..45000 -> (freq - 5950) / 5
            in 58320..70200 -> (freq - 56160) / 2160
            else -> throw IllegalArgumentException("Invalid frequency $freq")
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
        @get:RequiresApi(31)
        private val getChannels by lazy @TargetApi(31) {
            SoftApConfiguration::class.java.getDeclaredMethod("getChannels")
        }
        @get:RequiresApi(31)
        private val getMacRandomizationSetting by lazy @TargetApi(31) {
            SoftApConfiguration::class.java.getDeclaredMethod("getMacRandomizationSetting")
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
        @get:RequiresApi(31)
        private val isBridgedModeOpportunisticShutdownEnabled by lazy @TargetApi(31) {
            SoftApConfiguration::class.java.getDeclaredMethod("isBridgedModeOpportunisticShutdownEnabled")
        }
        @get:RequiresApi(30)
        private val isClientControlByUserEnabled by lazy @TargetApi(30) {
            SoftApConfiguration::class.java.getDeclaredMethod("isClientControlByUserEnabled")
        }
        @get:RequiresApi(31)
        private val isIeee80211axEnabled by lazy @TargetApi(31) {
            SoftApConfiguration::class.java.getDeclaredMethod("isIeee80211axEnabled")
        }
        @get:RequiresApi(31)
        private val isUserConfiguration by lazy @TargetApi(31) {
            SoftApConfiguration::class.java.getDeclaredMethod("isUserConfiguration")
        }

        @get:RequiresApi(30)
        private val classBuilder by lazy { Class.forName("android.net.wifi.SoftApConfiguration\$Builder") }
        @get:RequiresApi(30)
        private val newBuilder by lazy @TargetApi(30) { classBuilder.getConstructor(SoftApConfiguration::class.java) }
        @get:RequiresApi(30)
        private val build by lazy @TargetApi(30) { classBuilder.getDeclaredMethod("build") }
        @get:RequiresApi(30)
        private val setAllowedClientList by lazy @TargetApi(30) {
            classBuilder.getDeclaredMethod("setAllowedClientList", java.util.List::class.java)
        }
        @get:RequiresApi(30)
        private val setAutoShutdownEnabled by lazy @TargetApi(30) {
            classBuilder.getDeclaredMethod("setAutoShutdownEnabled", Boolean::class.java)
        }
        @get:RequiresApi(30)
        private val setBand by lazy @TargetApi(30) { classBuilder.getDeclaredMethod("setBand", Int::class.java) }
        @get:RequiresApi(30)
        private val setBlockedClientList by lazy @TargetApi(30) {
            classBuilder.getDeclaredMethod("setBlockedClientList", java.util.List::class.java)
        }
        @get:RequiresApi(31)
        private val setBridgedModeOpportunisticShutdownEnabled by lazy @TargetApi(31) {
            classBuilder.getDeclaredMethod("setBridgedModeOpportunisticShutdownEnabled", Boolean::class.java)
        }
        @get:RequiresApi(30)
        private val setBssid by lazy @TargetApi(30) {
            classBuilder.getDeclaredMethod("setBssid", MacAddress::class.java)
        }
        @get:RequiresApi(30)
        private val setChannel by lazy @TargetApi(30) {
            classBuilder.getDeclaredMethod("setChannel", Int::class.java, Int::class.java)
        }
        @get:RequiresApi(31)
        private val setChannels by lazy @TargetApi(31) {
            classBuilder.getDeclaredMethod("setChannels", SparseIntArray::class.java)
        }
        @get:RequiresApi(30)
        private val setClientControlByUserEnabled by lazy @TargetApi(30) {
            classBuilder.getDeclaredMethod("setClientControlByUserEnabled", Boolean::class.java)
        }
        @get:RequiresApi(30)
        private val setHiddenSsid by lazy @TargetApi(30) {
            classBuilder.getDeclaredMethod("setHiddenSsid", Boolean::class.java)
        }
        @get:RequiresApi(31)
        private val setIeee80211axEnabled by lazy @TargetApi(31) {
            classBuilder.getDeclaredMethod("setIeee80211axEnabled", Boolean::class.java)
        }
        @get:RequiresApi(31)
        private val setMacRandomizationSetting by lazy @TargetApi(31) {
            classBuilder.getDeclaredMethod("setMacRandomizationSetting", Int::class.java)
        }
        @get:RequiresApi(30)
        private val setMaxNumberOfClients by lazy @TargetApi(31) {
            classBuilder.getDeclaredMethod("setMaxNumberOfClients", Int::class.java)
        }
        @get:RequiresApi(30)
        private val setPassphrase by lazy @TargetApi(30) {
            classBuilder.getDeclaredMethod("setPassphrase", String::class.java, Int::class.java)
        }
        @get:RequiresApi(30)
        private val setShutdownTimeoutMillis by lazy @TargetApi(30) {
            classBuilder.getDeclaredMethod("setShutdownTimeoutMillis", Long::class.java)
        }
        @get:RequiresApi(30)
        private val setSsid by lazy @TargetApi(30) { classBuilder.getDeclaredMethod("setSsid", String::class.java) }
        @get:RequiresApi(31)
        private val setUserConfiguration by lazy @TargetApi(31) { UnblockCentral.setUserConfiguration(classBuilder) }

        @Deprecated("Class deprecated in framework")
        @Suppress("DEPRECATION")
        fun android.net.wifi.WifiConfiguration.toCompat() = SoftApConfigurationCompat(
                SSID,
                BSSID?.let { MacAddressCompat.fromString(it) }?.addr,
                preSharedKey,
                hiddenSSID,
                // https://cs.android.com/android/platform/superproject/+/master:frameworks/base/wifi/java/android/net/wifi/SoftApConfToXmlMigrationUtil.java;l=87;drc=aa6527cf41671d1ed417b8ebdb6b3aa614f62344
                SparseIntArray(1).also {
                    if (Build.VERSION.SDK_INT >= 23) it.append(when (val band = apBand.getInt(this)) {
                        0 -> BAND_2GHZ
                        1 -> BAND_5GHZ
                        -1 -> BAND_LEGACY
                        else -> throw IllegalArgumentException("Unexpected band $band")
                    }, apChannel.getInt(this)) else it.append(BAND_LEGACY, 0)
                },
                allowedKeyManagement.nextSetBit(0).let { selected ->
                    require(allowedKeyManagement.nextSetBit(selected + 1) < 0) {
                        "More than 1 key managements supplied: $allowedKeyManagement"
                    }
                    when (if (selected < 0) -1 else selected) {
                        -1,     // getAuthType returns NONE if nothing is selected
                        android.net.wifi.WifiConfiguration.KeyMgmt.NONE -> SoftApConfiguration.SECURITY_TYPE_OPEN
                        android.net.wifi.WifiConfiguration.KeyMgmt.WPA_PSK,
                        LEGACY_WPA2_PSK,
                        6,      // FT_PSK
                        11 -> { // WPA_PSK_SHA256
                            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
                        }
                        android.net.wifi.WifiConfiguration.KeyMgmt.SAE -> SoftApConfiguration.SECURITY_TYPE_WPA3_SAE
                        else -> android.net.wifi.WifiConfiguration.KeyMgmt.strings
                                .getOrElse<String>(selected) { "?" }.let {
                            throw IllegalArgumentException("Unrecognized key management $it ($selected)")
                        }
                    }
                },
                isAutoShutdownEnabled = if (Build.VERSION.SDK_INT >= 28) TetherTimeoutMonitor.enabled else false,
                underlying = this)

        @RequiresApi(30)
        @Suppress("UNCHECKED_CAST")
        fun SoftApConfiguration.toCompat() = SoftApConfigurationCompat(
            ssid,
            bssid?.toCompat()?.addr,
            passphrase,
            isHiddenSsid,
            if (Build.VERSION.SDK_INT >= 31) getChannels(this) as SparseIntArray else SparseIntArray(1).also {
                it.append(getBand(this) as Int, getChannel(this) as Int)
            },
            securityType,
            getMaxNumberOfClients(this) as Int,
            isAutoShutdownEnabled(this) as Boolean,
            getShutdownTimeoutMillis(this) as Long,
            isClientControlByUserEnabled(this) as Boolean,
            getBlockedClientList(this) as List<MacAddress>,
            getAllowedClientList(this) as List<MacAddress>,
            if (Build.VERSION.SDK_INT >= 31) getMacRandomizationSetting(this) as Int else RANDOMIZATION_PERSISTENT,
            Build.VERSION.SDK_INT < 31 || isBridgedModeOpportunisticShutdownEnabled(this) as Boolean,
            Build.VERSION.SDK_INT < 31 || isIeee80211axEnabled(this) as Boolean,
            Build.VERSION.SDK_INT < 31 || isUserConfiguration(this) as Boolean,
            this,
        )

        /**
         * Only single band/channel can be supplied on API 23-30
         */
        fun requireSingleBand(channels: SparseIntArray): Pair<Int, Int> {
            require(channels.size() == 1) { "Unsupported number of bands configured" }
            return channels.keyAt(0) to channels.valueAt(0)
        }

        @RequiresApi(30)
        private fun setChannelsCompat(builder: Any, channels: SparseIntArray) = if (Build.VERSION.SDK_INT < 31) {
            val (band, channel) = requireSingleBand(channels)
            if (channel == 0) setBand(builder, band) else setChannel(builder, channel, band)
        } else setChannels(builder, channels)
        @get:RequiresApi(30)
        private val staticBuilder by lazy @TargetApi(30) { classBuilder.newInstance() }
        @RequiresApi(30)
        fun testPlatformValidity(channels: SparseIntArray) = setChannelsCompat(staticBuilder, channels)
        @RequiresApi(30)
        fun testPlatformValidity(bssid: MacAddress) = setBssid(staticBuilder, bssid)
    }

    @Suppress("DEPRECATION")
    inline var bssid: MacAddressCompat?
        get() = bssidAddr?.let { MacAddressCompat(it) }
        set(value) {
            bssidAddr = value?.addr
        }

    fun setChannel(channel: Int, band: Int = BAND_LEGACY) {
        channels = SparseIntArray(1).apply {
            append(when {
                channel <= 0 || band != BAND_LEGACY -> band
                channel > 14 -> BAND_5GHZ
                else -> BAND_2GHZ
            }, channel)
        }
    }

    fun setMacRandomizationEnabled(enabled: Boolean) {
        macRandomizationSetting = if (enabled) RANDOMIZATION_PERSISTENT else RANDOMIZATION_NONE
    }

    /**
     * Based on:
     * https://android.googlesource.com/platform/packages/apps/Settings/+/android-5.0.0_r1/src/com/android/settings/wifi/WifiApDialog.java#88
     * https://android.googlesource.com/platform/packages/apps/Settings/+/b1af85d/src/com/android/settings/wifi/tether/WifiTetherSettings.java#162
     * https://android.googlesource.com/platform/frameworks/base/+/92c8f59/wifi/java/android/net/wifi/SoftApConfiguration.java#511
     */
    @SuppressLint("NewApi") // https://android.googlesource.com/platform/frameworks/base/+/android-5.0.0_r1/wifi/java/android/net/wifi/WifiConfiguration.java#1385
    @Deprecated("Class deprecated in framework, use toPlatform().toWifiConfiguration()")
    @Suppress("DEPRECATION")
    fun toWifiConfiguration(): android.net.wifi.WifiConfiguration {
        val (band, channel) = requireSingleBand(channels)
        val wc = underlying as? android.net.wifi.WifiConfiguration
        val result = if (wc == null) android.net.wifi.WifiConfiguration() else android.net.wifi.WifiConfiguration(wc)
        val original = wc?.toCompat()
        result.SSID = ssid
        result.preSharedKey = passphrase
        result.hiddenSSID = isHiddenSsid
        if (Build.VERSION.SDK_INT >= 23) {
            apBand.setInt(result, when (band) {
                BAND_2GHZ -> 0
                BAND_5GHZ -> 1
                else -> {
                    require(Build.VERSION.SDK_INT >= 28) { "A band must be specified on this platform" }
                    require(isLegacyEitherBand(band)) { "Convert fail, unsupported band setting :$band" }
                    -1
                }
            })
            apChannel.setInt(result, channel)
        } else require(isLegacyEitherBand(band)) { "Specifying band is unsupported on this platform" }
        if (original?.securityType != securityType) {
            result.allowedKeyManagement.clear()
            result.allowedKeyManagement.set(when (securityType) {
                SoftApConfiguration.SECURITY_TYPE_OPEN -> android.net.wifi.WifiConfiguration.KeyMgmt.NONE
                // not actually used on API 30-
                SoftApConfiguration.SECURITY_TYPE_WPA2_PSK -> LEGACY_WPA2_PSK
                // CHANGED: not actually converted in framework-wifi
                SoftApConfiguration.SECURITY_TYPE_WPA3_SAE,
                SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION -> android.net.wifi.WifiConfiguration.KeyMgmt.SAE
                else -> throw IllegalArgumentException("Convert fail, unsupported security type :$securityType")
            })
            result.allowedAuthAlgorithms.clear()
            result.allowedAuthAlgorithms.set(android.net.wifi.WifiConfiguration.AuthAlgorithm.OPEN)
        }
        // CHANGED: not actually converted in framework-wifi
        if (bssid != original?.bssid) result.BSSID = bssid?.toString()
        return result
    }

    @RequiresApi(30)
    fun toPlatform(): SoftApConfiguration {
        val sac = underlying as? SoftApConfiguration
        val builder = if (sac == null) classBuilder.newInstance() else newBuilder.newInstance(sac)
        setSsid(builder, ssid)
        setPassphrase(builder, if (securityType == SoftApConfiguration.SECURITY_TYPE_OPEN) null else passphrase,
            securityType)
        setChannelsCompat(builder, channels)
        setBssid(builder, bssid?.toPlatform())
        setMaxNumberOfClients(builder, maxNumberOfClients)
        setShutdownTimeoutMillis(builder, shutdownTimeoutMillis)
        setAutoShutdownEnabled(builder, isAutoShutdownEnabled)
        setClientControlByUserEnabled(builder, isClientControlByUserEnabled)
        setHiddenSsid(builder, isHiddenSsid)
        setAllowedClientList(builder, allowedClientList)
        setBlockedClientList(builder, blockedClientList)
        if (Build.VERSION.SDK_INT >= 31) {
            setMacRandomizationSetting(builder, macRandomizationSetting)
            setBridgedModeOpportunisticShutdownEnabled(builder, isBridgedModeOpportunisticShutdownEnabled)
            setIeee80211axEnabled(builder, isIeee80211axEnabled)
            if (sac?.let { isUserConfiguration(it) as Boolean } != false != isUserConfiguration) try {
                setUserConfiguration(builder, isUserConfiguration)
            } catch (e: ReflectiveOperationException) {
                Timber.w(e) // as far as we are concerned, this field is not used anywhere so ignore for now
            }
        }
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
