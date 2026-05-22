package be.mygod.vpnhotspot.ui.apconfiguration

import android.net.MacAddress
import android.net.wifi.SoftApConfiguration
import android.net.wifi.p2p.WifiP2pGroup
import android.os.Build
import android.os.Parcelable
import androidx.compose.material3.SnackbarHostState
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.net.wifi.P2pSupplicantConfiguration
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat.Companion.toCompat
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiSsidCompat
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.ui.showLongSnackbar
import be.mygod.vpnhotspot.util.getRootCause
import be.mygod.vpnhotspot.util.readableMessage
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.lang.reflect.InvocationTargetException

@Parcelize
data class ApConfigurationSession(
    val initial: SoftApConfigurationCompat,
    val target: ApConfigurationTarget,
    val readOnly: Boolean = false,
) : Parcelable

enum class ApConfigurationTarget {
    System,
    Repeater,
    Temporary,
}

suspend fun loadSystemApConfiguration(snackbarHostState: SnackbarHostState): ApConfigurationSession? {
    return try {
        val config = if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
            WifiApManager.configurationLegacy?.toCompat() ?: SoftApConfigurationCompat()
        } else WifiApManager.configuration.toCompat()
        ApConfigurationSession(config, ApConfigurationTarget.System)
    } catch (e: InvocationTargetException) {
        if (e.targetException !is SecurityException) Timber.w(e)
        try {
            val config = if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
                RootManager.use { it.execute(WifiApCommands.GetConfigurationLegacy()) }?.toCompat()
                    ?: SoftApConfigurationCompat()
            } else RootManager.use { it.execute(WifiApCommands.GetConfiguration()) }.toCompat()
            ApConfigurationSession(config, ApConfigurationTarget.System)
        } catch (_: CancellationException) {
            null
        } catch (eRoot: Exception) {
            eRoot.addSuppressed(e)
            if (Build.VERSION.SDK_INT >= 30 || eRoot.getRootCause() !is SecurityException) Timber.w(eRoot)
            snackbarHostState.showLongSnackbar(eRoot.readableMessage)
            null
        }
    } catch (e: IllegalArgumentException) {
        Timber.w(e)
        snackbarHostState.showLongSnackbar(e.readableMessage)
        null
    }
}

suspend fun applySystemApConfiguration(
    configuration: SoftApConfigurationCompat,
    snackbarHostState: SnackbarHostState,
): Boolean {
    if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
        if (configuration.isAutoShutdownEnabled != TetherTimeoutMonitor.enabled) try {
            TetherTimeoutMonitor.setEnabled(configuration.isAutoShutdownEnabled)
        } catch (e: Exception) {
            Timber.w(e)
            snackbarHostState.showLongSnackbar(e.readableMessage)
        }
        val wc = configuration.toWifiConfiguration()
        try {
            if (WifiApManager.setConfiguration(wc)) return true
        } catch (e: InvocationTargetException) {
            try {
                if (RootManager.use { it.execute(WifiApCommands.SetConfigurationLegacy(wc)) }.value) return true
            } catch (eCancel: CancellationException) {
                snackbarHostState.showLongSnackbar(eCancel.readableMessage)
                return false
            } catch (eRoot: Exception) {
                eRoot.addSuppressed(e)
                Timber.w(eRoot)
                snackbarHostState.showLongSnackbar(eRoot.readableMessage)
                return false
            }
        }
    } else {
        val platform = try {
            configuration.toPlatform()
        } catch (e: InvocationTargetException) {
            Timber.w(e)
            snackbarHostState.showLongSnackbar(e.readableMessage)
            return false
        }
        try {
            if (WifiApManager.setConfiguration(platform)) return true
        } catch (e: InvocationTargetException) {
            try {
                if (RootManager.use { it.execute(WifiApCommands.SetConfiguration(platform)) }.value) return true
            } catch (eCancel: CancellationException) {
                snackbarHostState.showLongSnackbar(eCancel.readableMessage)
                return false
            } catch (eRoot: Exception) {
                eRoot.addSuppressed(e)
                Timber.w(eRoot)
                snackbarHostState.showLongSnackbar(eRoot.readableMessage)
                return false
            }
        }
    }
    snackbarHostState.showLongSnackbar(app.getString(R.string.configuration_rejected))
    return false
}

suspend fun loadRepeaterApConfiguration(
    binder: RepeaterService.Binder?,
    snackbarHostState: SnackbarHostState,
): ApConfigurationSession? {
    if (RepeaterService.safeMode) {
        val networkName = RepeaterService.networkName
        val passphrase = RepeaterService.passphrase
        if (networkName != null && passphrase != null) {
            val config = SoftApConfigurationCompat(
                ssid = networkName,
                passphrase = passphrase,
                securityType = RepeaterService.securityType,
                isAutoShutdownEnabled = RepeaterService.isAutoShutdownEnabled,
                shutdownTimeoutMillis = RepeaterService.shutdownTimeoutMillis,
                macRandomizationSetting = if (WifiApManager.p2pMacRandomizationSupported) {
                    SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT
                } else SoftApConfigurationCompat.RANDOMIZATION_NONE,
                vendorElements = RepeaterService.vendorElements,
            ).apply {
                bssid = RepeaterService.deviceAddress
                setChannel(RepeaterService.operatingChannel, RepeaterService.operatingBand)
            }
            return ApConfigurationSession(config, ApConfigurationTarget.Repeater)
        }
    } else if (binder != null) {
        val group = binder.group.value ?: binder.fetchPersistentGroup().let { binder.group.value }
        if (group != null) {
            var readOnly = false
            val config = SoftApConfigurationCompat(
                ssid = WifiSsidCompat.fromUtf8Text(group.networkName),
                securityType = if (Build.VERSION.SDK_INT >= 36) when (group.securityType) {
                    WifiP2pGroup.SECURITY_TYPE_WPA3_COMPATIBILITY -> SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION
                    WifiP2pGroup.SECURITY_TYPE_WPA3_SAE -> SoftApConfiguration.SECURITY_TYPE_WPA3_SAE
                    else -> SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
                } else SoftApConfiguration.SECURITY_TYPE_WPA2_PSK,
                isAutoShutdownEnabled = RepeaterService.isAutoShutdownEnabled,
                shutdownTimeoutMillis = RepeaterService.shutdownTimeoutMillis,
                macRandomizationSetting = if (WifiApManager.p2pMacRandomizationSupported) {
                    SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT
                } else SoftApConfigurationCompat.RANDOMIZATION_NONE,
                vendorElements = RepeaterService.vendorElements,
            ).apply {
                setChannel(RepeaterService.operatingChannel)
                try {
                    val parsed = P2pSupplicantConfiguration(group).also {
                        it.init(binder.obtainDeviceAddress()?.toString())
                    }
                    passphrase = parsed.psk
                    bssid = parsed.bssid
                } catch (e: Exception) {
                    if (e is P2pSupplicantConfiguration.LoggedException) Timber.d(e)
                    else if (e !is CancellationException) Timber.w(e)
                    readOnly = true
                    passphrase = group.passphrase
                    try {
                        bssid = group.owner?.deviceAddress?.let(MacAddress::fromString)
                    } catch (_: IllegalArgumentException) { }
                }
            }
            return ApConfigurationSession(
                config,
                ApConfigurationTarget.Repeater,
                readOnly = readOnly,
            )
        }
    }
    snackbarHostState.showLongSnackbar(app.getString(R.string.repeater_configure_failure))
    return null
}

suspend fun applyRepeaterApConfiguration(
    binder: RepeaterService.Binder?,
    config: SoftApConfigurationCompat,
    snackbarHostState: SnackbarHostState,
): Boolean {
    if (RepeaterService.safeMode) {
        RepeaterService.networkName = config.ssid
        RepeaterService.deviceAddress = config.bssid
        RepeaterService.passphrase = config.passphrase
        RepeaterService.securityType = config.securityType
        applyRepeaterCommonConfiguration(config)
        return true
    }
    if (binder == null) {
        snackbarHostState.showLongSnackbar(app.getString(R.string.repeater_configure_failure))
        return false
    }
    val group = binder.group.value ?: binder.fetchPersistentGroup().let { binder.group.value }
    if (group == null) {
        snackbarHostState.showLongSnackbar(app.getString(R.string.repeater_configure_failure))
        return false
    }
    val parsed = try {
        P2pSupplicantConfiguration(group).also { it.init(binder.obtainDeviceAddress()?.toString()) }
    } catch (e: CancellationException) {
        throw e
    } catch (e: Exception) {
        if (e is P2pSupplicantConfiguration.LoggedException) Timber.d(e) else Timber.w(e)
        snackbarHostState.showLongSnackbar(e.readableMessage)
        return false
    }
    val mayBeModified = parsed.psk != config.passphrase || parsed.bssid != config.bssid || config.ssid.run {
        if (this != null) decode().let { it == null || group.networkName != it }
        else group.networkName != null
    }
    if (mayBeModified) try {
        withContext(Dispatchers.Default) { parsed.update(config.ssid!!, config.passphrase!!, config.bssid) }
        binder.clearGroup()
    } catch (e: Exception) {
        Timber.w(e)
        snackbarHostState.showLongSnackbar(e.readableMessage)
    }
    applyRepeaterCommonConfiguration(config)
    return true
}

private fun applyRepeaterCommonConfiguration(config: SoftApConfigurationCompat) {
    val (band, channel) = SoftApConfigurationCompat.requireSingleBand(config.channels)
    RepeaterService.operatingBand = band
    RepeaterService.operatingChannel = channel
    RepeaterService.isAutoShutdownEnabled = config.isAutoShutdownEnabled
    RepeaterService.shutdownTimeoutMillis = config.shutdownTimeoutMillis
    RepeaterService.vendorElements = config.vendorElements
}
