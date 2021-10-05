package be.mygod.vpnhotspot.net.wifi

import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.content.ClipData
import android.content.DialogInterface
import android.net.wifi.SoftApConfiguration
import android.os.Build
import android.os.Parcelable
import android.text.Editable
import android.text.InputFilter
import android.text.TextWatcher
import android.util.Base64
import android.util.SparseIntArray
import android.view.MenuItem
import android.view.View
import android.widget.AdapterView
import android.widget.ArrayAdapter
import android.widget.Spinner
import android.widget.Toast
import androidx.annotation.RequiresApi
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.widget.Toolbar
import androidx.core.view.isGone
import be.mygod.librootkotlinx.toByteArray
import be.mygod.librootkotlinx.toParcelable
import be.mygod.vpnhotspot.AlertDialogFragment
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.databinding.DialogWifiApBinding
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.util.QRCodeDialog
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.util.showAllowingStateLoss
import kotlinx.parcelize.Parcelize
import timber.log.Timber

/**
 * Based on: https://android.googlesource.com/platform/packages/apps/Settings/+/39b4674/src/com/android/settings/wifi/WifiApDialog.java
 *
 * This dialog has been deprecated in API 28, but we are still using it since it works better for our purposes.
 * Related: https://android.googlesource.com/platform/packages/apps/Settings/+/defb1183ecb00d6231bac7d934d07f58f90261ea
 */
class WifiApDialogFragment : AlertDialogFragment<WifiApDialogFragment.Arg, WifiApDialogFragment.Arg>(), TextWatcher,
        Toolbar.OnMenuItemClickListener, AdapterView.OnItemSelectedListener {
    companion object {
        private const val BASE64_FLAGS = Base64.NO_PADDING or Base64.NO_WRAP
        private val nonMacChars = "[^0-9a-fA-F:]+".toRegex()
        private val baseOptions by lazy { listOf(ChannelOption.Disabled, ChannelOption.Auto) }
        private val channels2G by lazy {
            baseOptions + (1..14).map { ChannelOption(it, SoftApConfigurationCompat.BAND_2GHZ) }
        }
        private val channels5G by lazy {
            baseOptions + (1..196).map { ChannelOption(it, SoftApConfigurationCompat.BAND_5GHZ) }
        }
        @get:RequiresApi(30)
        private val channels6G by lazy {
            baseOptions + (1..233).map { ChannelOption(it, SoftApConfigurationCompat.BAND_6GHZ) }
        }
        @get:RequiresApi(31)
        private val channels60G by lazy {
            baseOptions + (1..6).map { ChannelOption(it, SoftApConfigurationCompat.BAND_60GHZ) }
        }
        /**
         * Source: https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/c2fc6a1/service/java/com/android/server/wifi/p2p/SupplicantP2pIfaceHal.java#1396
         */
        private val p2pChannels by lazy {
            baseOptions + (15..165).map { ChannelOption(it, SoftApConfigurationCompat.BAND_5GHZ) }
        }
    }

    @Parcelize
    data class Arg(val configuration: SoftApConfigurationCompat,
                   val readOnly: Boolean = false,
                   /**
                    * KeyMgmt is enforced to WPA_PSK.
                    * Various values for apBand are allowed according to different rules.
                    */
                   val p2pMode: Boolean = false) : Parcelable

    private open class ChannelOption(val channel: Int = 0, private val band: Int = 0) {
        object Disabled : ChannelOption(-1) {
            override fun toString() = app.getString(R.string.wifi_ap_choose_disabled)
        }
        object Auto : ChannelOption() {
            override fun toString() = app.getString(R.string.wifi_ap_choose_auto)
        }
        override fun toString() = "${SoftApConfigurationCompat.channelToFrequency(band, channel)} MHz ($channel)"
    }

    private lateinit var dialogView: DialogWifiApBinding
    private lateinit var base: SoftApConfigurationCompat
    private var started = false
    private val currentChannels5G get() = if (arg.p2pMode && !RepeaterService.safeMode) p2pChannels else channels5G
    override val ret get() = Arg(generateConfig())

    private fun generateChannels() = SparseIntArray(4).apply {
        for ((band, spinner) in arrayOf(SoftApConfigurationCompat.BAND_2GHZ to dialogView.band2G,
            SoftApConfigurationCompat.BAND_5GHZ to dialogView.band5G,
            SoftApConfigurationCompat.BAND_6GHZ to dialogView.band6G,
            SoftApConfigurationCompat.BAND_60GHZ to dialogView.band60G)) {
            val channel = (spinner.selectedItem as ChannelOption?)?.channel
            if (channel != null && channel >= 0) append(band, channel)
        }
    }.let {
        if (arg.p2pMode || Build.VERSION.SDK_INT < 31 || !dialogView.bridgedMode.isChecked || it.size() > 2) {
            SoftApConfigurationCompat.optimizeChannels(it)
        } else it
    }
    private fun generateConfig(full: Boolean = true) = base.copy(
            ssid = dialogView.ssid.text.toString(),
            passphrase = if (dialogView.password.length() != 0) dialogView.password.text.toString() else null).apply {
        if (!arg.p2pMode) {
            securityType = dialogView.security.selectedItemPosition
            isHiddenSsid = dialogView.hiddenSsid.isChecked
        }
        if (full) @TargetApi(28) {
            isAutoShutdownEnabled = dialogView.autoShutdown.isChecked
            shutdownTimeoutMillis = dialogView.timeout.text.let { text ->
                if (text.isNullOrEmpty()) 0 else text.toString().toLong()
            }
            if (Build.VERSION.SDK_INT >= 23 || arg.p2pMode) channels = generateChannels()
            bssid = if (dialogView.bssid.length() != 0) {
                MacAddressCompat.fromString(dialogView.bssid.text.toString())
            } else null
            maxNumberOfClients = dialogView.maxClient.text.let { text ->
                if (text.isNullOrEmpty()) 0 else text.toString().toInt()
            }
            isClientControlByUserEnabled = dialogView.clientUserControl.isChecked
            allowedClientList = (dialogView.allowedList.text ?: "").split(nonMacChars)
                    .filter { it.isNotEmpty() }.map { MacAddressCompat.fromString(it).toPlatform() }
            blockedClientList = (dialogView.blockedList.text ?: "").split(nonMacChars)
                    .filter { it.isNotEmpty() }.map { MacAddressCompat.fromString(it).toPlatform() }
            setMacRandomizationEnabled(dialogView.macRandomization.isChecked)
            isBridgedModeOpportunisticShutdownEnabled = dialogView.bridgedModeOpportunisticShutdown.isChecked
            isIeee80211axEnabled = dialogView.ieee80211ax.isChecked
            isUserConfiguration = dialogView.userConfig.isChecked
        }
    }

    override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
        val activity = requireActivity()
        @SuppressLint("InflateParams")
        dialogView = DialogWifiApBinding.inflate(activity.layoutInflater)
        setView(dialogView.root)
        if (!arg.readOnly) setPositiveButton(R.string.wifi_save, listener)
        setNegativeButton(R.string.donations__button_close, null)
        dialogView.toolbar.inflateMenu(R.menu.toolbar_configuration)
        dialogView.toolbar.setOnMenuItemClickListener(this@WifiApDialogFragment)
        if (!arg.readOnly) dialogView.ssid.addTextChangedListener(this@WifiApDialogFragment)
        if (arg.p2pMode) dialogView.securityWrapper.isGone = true else dialogView.security.apply {
            adapter = ArrayAdapter(activity, android.R.layout.simple_spinner_item, 0,
                    SoftApConfigurationCompat.securityTypes).apply {
                setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
            }
            onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
                override fun onNothingSelected(parent: AdapterView<*>?) = error("Must select something")
                override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
                    if (position != SoftApConfiguration.SECURITY_TYPE_OPEN) {
                        dialogView.passwordWrapper.isGone = false
                        if (position == SoftApConfiguration.SECURITY_TYPE_WPA3_SAE) {
                            dialogView.passwordWrapper.isCounterEnabled = false
                            dialogView.passwordWrapper.counterMaxLength = 0
                            dialogView.password.filters = emptyArray()
                        } else {
                            dialogView.passwordWrapper.isCounterEnabled = true
                            dialogView.passwordWrapper.counterMaxLength = 63
                            dialogView.password.filters = arrayOf(InputFilter.LengthFilter(63))
                        }
                    } else dialogView.passwordWrapper.isGone = true
                    validate()
                }
            }
        }
        if (!arg.readOnly) dialogView.password.addTextChangedListener(this@WifiApDialogFragment)
        if (!arg.p2pMode && Build.VERSION.SDK_INT < 28) dialogView.autoShutdown.isGone = true
        if (arg.p2pMode || Build.VERSION.SDK_INT >= 30) {
            dialogView.timeoutWrapper.helperText = getString(R.string.wifi_hotspot_timeout_default,
                    TetherTimeoutMonitor.defaultTimeout)
            if (!arg.readOnly) dialogView.timeout.addTextChangedListener(this@WifiApDialogFragment)
        } else dialogView.timeoutWrapper.isGone = true
        fun Spinner.configure(options: List<ChannelOption>) {
            adapter = ArrayAdapter(activity, android.R.layout.simple_spinner_item, 0, options).apply {
                setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
            }
            if (!arg.readOnly) onItemSelectedListener = this@WifiApDialogFragment
        }
        if (Build.VERSION.SDK_INT >= 23 || arg.p2pMode) {
            dialogView.band2G.configure(channels2G)
            dialogView.band5G.configure(currentChannels5G)
            if (Build.VERSION.SDK_INT >= 30 && !arg.p2pMode) dialogView.band6G.configure(channels6G)
            else dialogView.bandWrapper6G.isGone = true
            if (Build.VERSION.SDK_INT >= 31 && !arg.p2pMode) dialogView.band60G.configure(channels60G) else {
                dialogView.bandWrapper60G.isGone = true
                dialogView.bridgedMode.isGone = true
                dialogView.bridgedModeOpportunisticShutdown.isGone = true
            }
        } else dialogView.bandGroup.isGone = true
        if (arg.p2pMode || Build.VERSION.SDK_INT < 30) dialogView.accessControlGroup.isGone = true
        else if (!arg.readOnly) {
            dialogView.maxClient.addTextChangedListener(this@WifiApDialogFragment)
            dialogView.blockedList.addTextChangedListener(this@WifiApDialogFragment)
            dialogView.allowedList.addTextChangedListener(this@WifiApDialogFragment)
        }
        if (!arg.readOnly) dialogView.bssid.addTextChangedListener(this@WifiApDialogFragment)
        if (arg.p2pMode) dialogView.hiddenSsid.isGone = true
        if (arg.p2pMode && Build.VERSION.SDK_INT >= 29) dialogView.macRandomization.isEnabled = false
        else if (arg.p2pMode || Build.VERSION.SDK_INT < 31) dialogView.macRandomization.isGone = true
        if (arg.p2pMode || Build.VERSION.SDK_INT < 31) {
            dialogView.ieee80211ax.isGone = true
            dialogView.userConfig.isGone = true
        }
        base = arg.configuration
        populateFromConfiguration()
    }

    private fun locate(band: Int, channels: List<ChannelOption>): Int {
        val channel = base.getChannel(band)
        val selection = channels.indexOfFirst { it.channel == channel }
        return if (selection == -1) {
            Timber.w(Exception("Unable to locate $band, $channel, ${arg.p2pMode && !RepeaterService.safeMode}"))
            0
        } else selection
    }
    private var userBridgedMode = false
    private fun setBridgedMode(): Boolean {
        var auto = 0
        var set = 0
        for (s in arrayOf(dialogView.band2G, dialogView.band5G, dialogView.band6G,
            dialogView.band60G)) when (s.selectedItem) {
            is ChannelOption.Auto -> auto = 1
            !is ChannelOption.Disabled -> ++set
        }
        if (auto + set > 1) {
            if (dialogView.bridgedMode.isEnabled) {
                userBridgedMode = dialogView.bridgedMode.isChecked
                dialogView.bridgedMode.isEnabled = false
                dialogView.bridgedMode.isChecked = true
            }
        } else if (!dialogView.bridgedMode.isEnabled) {
            dialogView.bridgedMode.isEnabled = true
            dialogView.bridgedMode.isChecked = userBridgedMode
        }
        return auto + set > 0
    }
    private fun populateFromConfiguration() {
        dialogView.ssid.setText(base.ssid)
        if (!arg.p2pMode) dialogView.security.setSelection(base.securityType)
        dialogView.password.setText(base.passphrase)
        dialogView.autoShutdown.isChecked = base.isAutoShutdownEnabled
        dialogView.timeout.setText(base.shutdownTimeoutMillis.let { if (it == 0L) "" else it.toString() })
        if (Build.VERSION.SDK_INT >= 23 || arg.p2pMode) {
            dialogView.band2G.setSelection(locate(SoftApConfigurationCompat.BAND_2GHZ, channels2G))
            dialogView.band5G.setSelection(locate(SoftApConfigurationCompat.BAND_5GHZ, currentChannels5G))
            dialogView.band6G.setSelection(locate(SoftApConfigurationCompat.BAND_6GHZ, channels6G))
            dialogView.band60G.setSelection(locate(SoftApConfigurationCompat.BAND_60GHZ, channels60G))
            userBridgedMode = base.channels.size() > 1
            dialogView.bridgedMode.isChecked = userBridgedMode
            setBridgedMode()
        }
        dialogView.bssid.setText(base.bssid?.toString())
        dialogView.hiddenSsid.isChecked = base.isHiddenSsid
        dialogView.maxClient.setText(base.maxNumberOfClients.let { if (it == 0) "" else it.toString() })
        dialogView.clientUserControl.isChecked = base.isClientControlByUserEnabled
        dialogView.blockedList.setText(base.blockedClientList.joinToString("\n"))
        dialogView.allowedList.setText(base.allowedClientList.joinToString("\n"))
        dialogView.macRandomization.isChecked =
            base.macRandomizationSetting == SoftApConfigurationCompat.RANDOMIZATION_PERSISTENT
        dialogView.bridgedModeOpportunisticShutdown.isChecked = base.isBridgedModeOpportunisticShutdownEnabled
        dialogView.ieee80211ax.isChecked = base.isIeee80211axEnabled
        dialogView.userConfig.isChecked = base.isUserConfiguration
    }

    override fun onStart() {
        super.onStart()
        started = true
        validate()
    }

    @TargetApi(28)
    private fun validate() {
        if (!started) return
        val ssidLength = dialogView.ssid.text.toString().toByteArray().size
        dialogView.ssidWrapper.error = if (arg.p2pMode && RepeaterService.safeMode && ssidLength < 9) {
            requireContext().getString(R.string.settings_service_repeater_safe_mode_warning)
        } else null
        val selectedSecurity = if (arg.p2pMode) {
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
        } else dialogView.security.selectedItemPosition
        // see also: https://android.googlesource.com/platform/frameworks/base/+/92c8f59/wifi/java/android/net/wifi/SoftApConfiguration.java#688
        val passwordValid = when (selectedSecurity) {
            SoftApConfiguration.SECURITY_TYPE_OPEN -> true
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK, SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION -> {
                dialogView.password.length() in 8..63
            }
            else -> dialogView.password.length() > 0
        }
        dialogView.passwordWrapper.error = if (passwordValid) null else " "
        val timeoutError = dialogView.timeout.text.let { text ->
            if (text.isNullOrEmpty()) null else try {
                text.toString().toLong()
                null
            } catch (e: NumberFormatException) {
                e.readableMessage
            }
        }
        dialogView.timeoutWrapper.error = timeoutError
        val bandError = if (arg.p2pMode || Build.VERSION.SDK_INT < 30) {
            val option5G = dialogView.band5G.selectedItem
            val valid = when (dialogView.band2G.selectedItem) {
                is ChannelOption.Disabled -> option5G !is ChannelOption.Disabled &&
                        (!arg.p2pMode || RepeaterService.safeMode || option5G !is ChannelOption.Auto)
                is ChannelOption.Auto ->
                    (arg.p2pMode || Build.VERSION.SDK_INT >= 28) && option5G is ChannelOption.Auto ||
                            (!arg.p2pMode || RepeaterService.safeMode) && option5G is ChannelOption.Disabled
                else -> option5G is ChannelOption.Disabled
            }
            if (valid) null else ""
        } else {
            if (Build.VERSION.SDK_INT >= 31) setBridgedMode()
            try {
                SoftApConfigurationCompat.testPlatformValidity(generateChannels())
                null
            } catch (e: Exception) {
                e.readableMessage
            }
        }
        dialogView.bandError.isGone = bandError.isNullOrEmpty()
        dialogView.bandError.text = bandError
        dialogView.bssidWrapper.error = null
        val bssidValid = dialogView.bssid.length() == 0 || try {
            MacAddressCompat.fromString(dialogView.bssid.text.toString())
            true
        } catch (e: IllegalArgumentException) {
            dialogView.bssidWrapper.error = e.readableMessage
            false
        }
        val maxClientError = dialogView.maxClient.text.let { text ->
            if (text.isNullOrEmpty()) null else try {
                text.toString().toInt()
                null
            } catch (e: NumberFormatException) {
                e.readableMessage
            }
        }
        dialogView.maxClientWrapper.error = maxClientError
        val listsNoError = if (Build.VERSION.SDK_INT >= 30) {
            val (blockedList, blockedListError) = try {
                (dialogView.blockedList.text ?: "").split(nonMacChars)
                    .filter { it.isNotEmpty() }.map { MacAddressCompat.fromString(it).toPlatform() }.toSet() to null
            } catch (e: IllegalArgumentException) {
                null to e.readableMessage
            }
            dialogView.blockedListWrapper.error = blockedListError
            val allowedListError = try {
                (dialogView.allowedList.text ?: "").split(nonMacChars).filter { it.isNotEmpty() }.forEach {
                    val mac = MacAddressCompat.fromString(it).toPlatform()
                    require(blockedList?.contains(mac) != true) { "A MAC address exists in both client lists" }
                }
                null
            } catch (e: IllegalArgumentException) {
                e.readableMessage
            }
            dialogView.allowedListWrapper.error = allowedListError
            blockedListError == null && allowedListError == null
        } else true
        val canCopy = timeoutError == null && bssidValid && maxClientError == null && listsNoError
        (dialog as? AlertDialog)?.getButton(DialogInterface.BUTTON_POSITIVE)?.isEnabled =
                ssidLength in 1..32 && passwordValid && bandError == null && canCopy
        dialogView.toolbar.menu.findItem(android.R.id.copy).isEnabled = canCopy
    }

    override fun onTextChanged(s: CharSequence, start: Int, before: Int, count: Int) { }
    override fun beforeTextChanged(s: CharSequence, start: Int, count: Int, after: Int) { }
    override fun afterTextChanged(editable: Editable) = validate()

    override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) = validate()
    override fun onNothingSelected(parent: AdapterView<*>?) = error("unreachable")

    override fun onMenuItemClick(item: MenuItem?): Boolean {
        return when (item?.itemId) {
            android.R.id.copy -> {
                app.clipboard.setPrimaryClip(ClipData.newPlainText(null,
                        Base64.encodeToString(generateConfig().toByteArray(), BASE64_FLAGS)))
                true
            }
            android.R.id.paste -> try {
                app.clipboard.primaryClip?.getItemAt(0)?.text?.apply {
                    Base64.decode(toString(), BASE64_FLAGS).toParcelable<SoftApConfigurationCompat>()?.let { config ->
                        val newUnderlying = config.underlying
                        if (newUnderlying != null) {
                            arg.configuration.underlying?.let { check(it.javaClass == newUnderlying.javaClass) }
                        } else config.underlying = arg.configuration.underlying
                        base = config
                        populateFromConfiguration()
                    }
                }
                true
            } catch (e: RuntimeException) {
                Toast.makeText(context, e.readableMessage, Toast.LENGTH_LONG).show()
                false
            }
            R.id.share_qr -> {
                QRCodeDialog().withArg(generateConfig(false).toQrCode()).showAllowingStateLoss(parentFragmentManager)
                true
            }
            else -> false
        }
    }
}
