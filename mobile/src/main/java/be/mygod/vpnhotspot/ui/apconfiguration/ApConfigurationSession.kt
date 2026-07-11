package be.mygod.vpnhotspot.ui.apconfiguration

import android.net.wifi.SoftApConfiguration
import android.net.wifi.p2p.WifiP2pGroup
import android.os.Build
import android.os.Parcelable
import androidx.compose.material3.SnackbarHostState
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
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
        } catch (e: CancellationException) {
            throw e
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
                throw eCancel
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
                throw eCancel
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

suspend fun loadRepeaterApConfiguration(binder: RepeaterService.Binder?): ApConfigurationSession {
    // nothing persisted yet: adopt the persistent group the framework would reuse on a direct start
    val adopted = if (RepeaterService.networkName == null) binder?.obtainGroup() else null
    val config = SoftApConfigurationCompat(
        ssid = adopted?.let { WifiSsidCompat.fromUtf8Text(it.networkName) } ?: RepeaterService.networkName,
        passphrase = adopted?.passphrase ?: RepeaterService.passphrase,
        securityType = if (adopted != null && Build.VERSION.SDK_INT >= 36) {
            adopted.securityType + SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
        } else RepeaterService.securityType,
        isAutoShutdownEnabled = RepeaterService.isAutoShutdownEnabled,
        shutdownTimeoutMillis = RepeaterService.shutdownTimeoutMillis,
        macRandomizationSetting = if (RepeaterService.randomizeMac && WifiApManager.p2pMacRandomizationSupported) {
            SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT
        } else SoftApConfigurationCompat.RANDOMIZATION_NONE,
        vendorElements = RepeaterService.vendorElements,
        vendorData = RepeaterService.vendorData,
    ).apply {
        setChannel(RepeaterService.operatingChannel, RepeaterService.operatingBand)
    }
    return ApConfigurationSession(config, ApConfigurationTarget.Repeater)
}

fun applyRepeaterApConfiguration(
    config: SoftApConfigurationCompat,
    useFramework: Boolean,
    disablePowerSave: Boolean,
): Boolean {
    RepeaterService.useFramework = useFramework
    RepeaterService.networkName = config.ssid
    RepeaterService.randomizeMac = config.macRandomizationSetting != SoftApConfigurationCompat.RANDOMIZATION_NONE
    RepeaterService.passphrase = config.passphrase
    RepeaterService.securityType = config.securityType
    RepeaterService.vendorData = config.vendorData
    RepeaterService.disablePowerSave = disablePowerSave
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
