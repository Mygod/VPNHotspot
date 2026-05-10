package be.mygod.vpnhotspot.manage

import android.Manifest
import android.content.ComponentName
import android.content.DialogInterface
import android.content.Intent
import android.content.ServiceConnection
import android.net.MacAddress
import android.net.wifi.SoftApConfiguration
import android.net.wifi.p2p.WifiP2pGroup
import android.net.wifi.p2p.WifiP2pManager
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.os.Parcelable
import android.text.SpannableStringBuilder
import android.text.method.LinkMovementMethod
import android.view.WindowManager
import android.widget.EditText
import androidx.annotation.MainThread
import androidx.annotation.StringRes
import androidx.appcompat.app.AlertDialog
import androidx.databinding.BaseObservable
import androidx.lifecycle.Lifecycle
import androidx.fragment.app.viewModels
import androidx.lifecycle.ViewModel
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.repeatOnLifecycle
import androidx.lifecycle.withStarted
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.AlertDialogFragment
import be.mygod.vpnhotspot.Empty
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.databinding.ListitemRepeaterBinding
import be.mygod.vpnhotspot.net.wifi.P2pSupplicantConfiguration
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.WifiApDialogFragment
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiSsidCompat
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.formatAddresses
import be.mygod.vpnhotspot.util.showAllowingStateLoss
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.net.NetworkInterface
import java.net.SocketException

class RepeaterManager(private val parent: TetheringFragment) : Manager(), ServiceConnection {
    companion object {
        private val interfaceAddress by lazy { WifiP2pGroup::class.java.getDeclaredField("interfaceAddress") }
    }
    class ViewHolder(val binding: ListitemRepeaterBinding) : RecyclerView.ViewHolder(binding.root) {
        init {
            binding.addresses.movementMethod = LinkMovementMethod.getInstance()
        }
    }
    inner class Data : BaseObservable() {
        val switchEnabled: Boolean get() = when (binder.value?.status?.value) {
                RepeaterService.Status.IDLE, RepeaterService.Status.ACTIVE -> true
                else -> false
            }
        val serviceStarted: Boolean get() = when (binder.value?.status?.value) {
                RepeaterService.Status.STARTING, RepeaterService.Status.ACTIVE -> true
                else -> false
            }

        val title: CharSequence get() {
            binder.value?.group?.value?.frequency?.let {
                if (it != 0) return parent.getString(R.string.repeater_channel,
                        it, SoftApConfigurationCompat.frequencyToChannel(it))
            }
            return parent.getString(R.string.title_repeater)
        }
        val description: CharSequence get() = SpannableStringBuilder().let { result ->
            fun WifiP2pManager.test(@StringRes feature: Int, sdk: Int, action: (WifiP2pManager) -> Boolean) {
                try {
                    if (!action(this)) return
                    result.append(if (result.isEmpty()) parent.getText(R.string.repeater_features) else ", ")
                    result.append(parent.getText(feature))
                } catch (e: NoSuchMethodError) {
                    if (Build.VERSION.SDK_INT >= sdk) Timber.w(e)
                }
            }
            if (Build.VERSION.SDK_INT >= 30) Services.p2p?.apply {
                test(R.string.repeater_feature_set_vendor_elements, 33) { isSetVendorElementsSupported }
//                test(R.string.repeater_feature_channel_constrained_discovery, 33) { isChannelConstrainedDiscoverySupported }
                test(R.string.repeater_feature_group_client_removal, 33) { isGroupClientRemovalSupported }
//                test(R.string.repeater_feature_group_owner_ipv6_link_local_address_provided, 34) { isGroupOwnerIPv6LinkLocalAddressProvided }
                test(R.string.repeater_feature_pcc_mode, 36) { isPccModeSupported }
                test(R.string.repeater_feature_wifi_direct_r2, 36) { isWiFiDirectR2Supported }
            }
            val addresses = binder.value?.group?.value?.let { group ->
                try {
                    NetworkInterface.getByName(group.`interface`)
                } catch (_: SocketException) {
                    null
                } catch (e: Exception) {
                    Timber.w(e)
                    null
                }.formatAddresses(macOverride = if (Build.VERSION.SDK_INT >= 30) try {
                    (interfaceAddress[group] as ByteArray?)?.let(MacAddress::fromBytes)
                } catch (e: NoSuchFieldException) {
                    if (Build.VERSION.SDK_INT >= 34) Timber.w(e)
                    null
                } else null)
            } ?: ""
            if (addresses.isNotEmpty() && result.isNotEmpty()) result.appendLine()
            result.append(addresses)
        }

        fun toggle() {
            val binder = binder.value
            when (binder?.status?.value) {
                RepeaterService.Status.IDLE -> parent.startRepeater.launch(if (Build.VERSION.SDK_INT >= 33) {
                    Manifest.permission.NEARBY_WIFI_DEVICES
                } else Manifest.permission.ACCESS_FINE_LOCATION)
                RepeaterService.Status.ACTIVE -> binder.shutdown()
                else -> { }
            }
        }

        fun wps() {
            if (binder.value?.active == true) WpsDialogFragment().apply {
                key()
            }.showAllowingStateLoss(parent.parentFragmentManager)
        }
    }

    @Parcelize
    data class WpsRet(val pin: String?) : Parcelable
    class WpsDialogFragment : AlertDialogFragment<Empty, WpsRet>() {
        override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
            setTitle(R.string.repeater_wps_dialog_title)
            setView(R.layout.dialog_wps)
            setPositiveButton(android.R.string.ok, listener)
            setNegativeButton(android.R.string.cancel, null)
            setNeutralButton(R.string.repeater_wps_dialog_pbc, listener)
        }

        override val ret get() = WpsRet(dialog!!.findViewById<EditText>(android.R.id.edit)?.text?.toString())

        override fun onCreateDialog(savedInstanceState: Bundle?) = super.onCreateDialog(savedInstanceState).apply {
            window!!.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_VISIBLE)
        }
    }

    class ConfigHolder : ViewModel() {
        var config: P2pSupplicantConfiguration? = null
    }

    override val type get() = VIEW_TYPE_REPEATER
    private val data = Data()
    private val mutableBinder = MutableStateFlow<RepeaterService.Binder?>(null)
    val binder = mutableBinder.asStateFlow()
    private val holder by parent.viewModels<ConfigHolder>()

    init {
        ServiceForegroundConnector(parent, this, RepeaterService::class)
        parent.viewLifecycleOwner.lifecycleScope.launch {
            parent.viewLifecycleOwner.repeatOnLifecycle(Lifecycle.State.STARTED) {
                binder.collectLatest { service ->
                    if (service == null) {
                        data.notifyChange()
                        return@collectLatest
                    }
                    merge(service.status, service.group).collect { data.notifyChange() }
                }
            }
        }
        AlertDialogFragment.setResultListener<WifiApDialogFragment.Arg>(parent, javaClass.name) { which, ret ->
            if (which == DialogInterface.BUTTON_POSITIVE) GlobalScope.launch(Dispatchers.Main.immediate) {
                updateConfiguration(ret!!.configuration)
            }
        }
        AlertDialogFragment.setResultListener<WpsDialogFragment, WpsRet>(parent) { which, ret ->
            when (which) {
                DialogInterface.BUTTON_POSITIVE -> binder.value!!.startWps(ret!!.pin)
                DialogInterface.BUTTON_NEUTRAL -> binder.value!!.startWps(null)
            }
        }
    }

    private var configuring = false
    fun configure() {
        if (configuring) return
        configuring = true
        val owner = parent.viewLifecycleOwner
        owner.lifecycleScope.launch {
            val (config, readOnly) = getConfiguration() ?: return@launch
            owner.withStarted {
                WifiApDialogFragment().apply {
                    arg(WifiApDialogFragment.Arg(config, readOnly, true))
                    key(this@RepeaterManager.javaClass.name)
                }.showAllowingStateLoss(parent.parentFragmentManager)
            }
            configuring = false
        }
    }

    override fun bindTo(viewHolder: RecyclerView.ViewHolder) {
        (viewHolder as ViewHolder).binding.data = data
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        mutableBinder.value = service as RepeaterService.Binder
        data.notifyChange()
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        if (binder.value == null) return
        mutableBinder.value = null
        data.notifyChange()
    }

    @MainThread
    private suspend fun getConfiguration(): Pair<SoftApConfigurationCompat, Boolean>? {
        if (RepeaterService.safeMode) {
            val networkName = RepeaterService.networkName
            val passphrase = RepeaterService.passphrase
            if (networkName != null && passphrase != null) {
                return SoftApConfigurationCompat(
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
                } to false
            }
        } else binder.value?.let { binder ->
            val group = binder.group.value ?: binder.fetchPersistentGroup().let { binder.group.value }
            if (group != null) return SoftApConfigurationCompat(
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
            ).run {
                setChannel(RepeaterService.operatingChannel)
                try {
                    val config = P2pSupplicantConfiguration(group)
                    config.init(binder.obtainDeviceAddress()?.toString())
                    holder.config = config
                    passphrase = config.psk
                    bssid = config.bssid
                    this to false
                } catch (e: Exception) {
                    if (e is P2pSupplicantConfiguration.LoggedException) Timber.d(e)
                    else if (e !is CancellationException) Timber.w(e)
                    passphrase = group.passphrase
                    try {
                        bssid = group.owner?.deviceAddress?.let(MacAddress::fromString)
                    } catch (_: IllegalArgumentException) { }
                    this to true
                }
            }
        }
        SmartSnackbar.make(R.string.repeater_configure_failure).show()
        return null
    }
    private suspend fun updateConfiguration(config: SoftApConfigurationCompat) {
        val (band, channel) = SoftApConfigurationCompat.requireSingleBand(config.channels)
        if (RepeaterService.safeMode) {
            RepeaterService.networkName = config.ssid
            RepeaterService.deviceAddress = config.bssid
            RepeaterService.passphrase = config.passphrase
            RepeaterService.securityType = config.securityType
        } else holder.config?.let { master ->
            val binder = binder.value
            val mayBeModified = master.psk != config.passphrase || master.bssid != config.bssid || config.ssid.run {
                if (this != null) decode().let {
                    it == null || binder?.group?.value?.networkName != it
                } else binder?.group?.value?.networkName != null
            }
            if (mayBeModified) try {
                withContext(Dispatchers.Default) { master.update(config.ssid!!, config.passphrase!!, config.bssid) }
                (this.binder.value ?: binder)?.clearGroup()
            } catch (e: Exception) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }
            holder.config = null
        }
        RepeaterService.operatingBand = band
        RepeaterService.operatingChannel = channel
        RepeaterService.isAutoShutdownEnabled = config.isAutoShutdownEnabled
        RepeaterService.shutdownTimeoutMillis = config.shutdownTimeoutMillis
        RepeaterService.vendorElements = config.vendorElements
    }
}
