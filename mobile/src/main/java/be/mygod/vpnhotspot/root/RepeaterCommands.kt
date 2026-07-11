package be.mygod.vpnhotspot.root

import android.hardware.wifi.common.OuiKeyedData
import android.hardware.wifi.supplicant.IfaceType
import android.hardware.wifi.supplicant.ISupplicant
import android.hardware.wifi.supplicant.KeyMgmtMask
import android.hardware.wifi.supplicant.P2pAddGroupConfigurationParams
import android.net.MacAddress
import android.net.wifi.ScanResult
import android.net.wifi.p2p.WifiP2pManager
import android.os.Looper
import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.ParcelableInt
import be.mygod.librootkotlinx.ParcelableList
import be.mygod.librootkotlinx.ParcelableThrowable
import be.mygod.librootkotlinx.RootCommand
import be.mygod.librootkotlinx.RootCommandNoResult
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestDeviceAddress
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestPersistentGroupInfo
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.setVendorElements
import be.mygod.vpnhotspot.util.Services
import kotlinx.parcelize.Parcelize

object RepeaterCommands {
    @Parcelize
    class RequestDeviceAddress : RootCommand<MacAddress?> {
        override suspend fun execute() = Services.p2p!!.run { requestDeviceAddress(obtainChannel()) }
    }

    @Parcelize
    class RequestPersistentGroupInfo : RootCommand<ParcelableList> {
        override suspend fun execute() = Services.p2p!!.run {
            ParcelableList(requestPersistentGroupInfo(obtainChannel()).toList())
        }
    }

    @Parcelize
    @RequiresApi(33)
    data class SetVendorElements(private val ve: List<ScanResult.InformationElement>) : RootCommand<ParcelableInt?> {
        override suspend fun execute() = Services.p2p!!.run {
            setVendorElements(obtainChannel(), ve)?.let { ParcelableInt(it) }
        }
    }

    /**
     * What the live supplicant backend can carry, so the UI can disable unrepresentable options and group creation
     * can fail with a specific error. [aidlVersion] is the AIDL `interfaceVersion`, or null when only the legacy
     * HIDL HAL is present. WPA3 key management, the 6 GHz band shorthand and vendor data all arrived in AIDL v3.
     */
    @Parcelize
    data class SupplicantCapability(val aidlVersion: Int?) : Parcelable {
        inline val aidlV3 get() = aidlVersion != null && aidlVersion >= 3
        val label get() = if (aidlVersion == null) "HIDL" else "AIDL v$aidlVersion"
    }

    @Parcelize
    class QuerySupplicantCapability : RootCommand<SupplicantCapability> {
        override suspend fun execute() = SupplicantCapability(SupplicantAidl.instance?.interfaceVersion)
    }

    @Parcelize
    data class DisablePowerSave(private val groupIfName: String) : RootCommandNoResult {
        override suspend fun execute() = null.also {
            SupplicantAidl.instance?.requireP2pInterface()?.setPowerSave(groupIfName, false)
                ?: SupplicantP2pIface.disablePowerSave(groupIfName)
        }
    }

    @Parcelize
    class AddPersistentGroupWithConfig(
        private val ssid: ByteArray,
        private val passphrase: String,
        private val frequency: Int,
        private val keyMgmtMask: Int,
        private val randomizeMac: Boolean,
        private val vendorData: Array<OuiKeyedData>,
    ) : RootCommand<ParcelableThrowable?> {
        override suspend fun execute(): ParcelableThrowable? {
            val aidl = SupplicantAidl.instance
            val capability = SupplicantCapability(aidl?.interfaceVersion)
            if (!capability.aidlV3) {
                require(keyMgmtMask == KeyMgmtMask.WPA_PSK) {
                    "WPA3 is not supported by the ${capability.label} supplicant backend"
                }
                require(frequency != 6) {
                    "6 GHz automatic channel is not supported by the ${capability.label} supplicant backend"
                }
                require(vendorData.isEmpty()) {
                    "Vendor data is not supported by the ${capability.label} supplicant backend"
                }
            }
            if (aidl == null) return SupplicantP2pIface.addGroup(ssid, passphrase, frequency, randomizeMac)
            val p2pIface = aidl.requireP2pInterface()
            val macRandomizationError = try {   // best-effort, matching Framework mode's MAC randomization behaviour
                p2pIface.setMacRandomization(randomizeMac)
                null
            } catch (e: Exception) {
                e
            }
            try {
                if (p2pIface.interfaceVersion < 3) @Suppress("DEPRECATION") {
                    p2pIface.addGroupWithConfig(ssid, passphrase, true, frequency, ByteArray(6), false)
                } else p2pIface.addGroupWithConfigurationParams(P2pAddGroupConfigurationParams().also {
                    it.ssid = ssid
                    it.passphrase = passphrase
                    it.isPersistent = true
                    it.frequencyMHzOrBand = frequency
                    it.goInterfaceAddress = ByteArray(6)
                    it.keyMgmtMask = keyMgmtMask
                    if (vendorData.isNotEmpty()) it.vendorData = vendorData
                })
                return macRandomizationError?.let { ParcelableThrowable(it) }
            } catch (e: Throwable) {
                macRandomizationError?.let { e.addSuppressed(it) }
                throw e
            }
        }
    }

    private fun ISupplicant.requireP2pInterface() = listInterfaces().firstOrNull { it.type == IfaceType.P2P }?.let {
        getP2pInterface(it.name)
    } ?: error("No framework-owned P2P supplicant interface")

    private var channel: WifiP2pManager.Channel? = null
    private fun WifiP2pManager.obtainChannel(): WifiP2pManager.Channel {
        channel?.let { return it }
        return initialize(Services.context, Looper.getMainLooper()) { channel = null }.also {
            channel = it    // cache the instance until invalidated
        }
    }
}
