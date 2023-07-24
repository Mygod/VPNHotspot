package be.mygod.vpnhotspot.net.wifi

import android.annotation.SuppressLint
import android.content.ClipData
import android.content.ClipDescription
import android.content.DialogInterface
import android.net.MacAddress
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
import androidx.core.os.persistableBundleOf
import androidx.core.view.isGone
import be.mygod.librootkotlinx.toByteArray
import be.mygod.librootkotlinx.toParcelable
import be.mygod.vpnhotspot.AlertDialogFragment
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.databinding.DialogWifiApBinding
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.util.QRCodeDialog
import be.mygod.vpnhotspot.util.RangeInput
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.util.showAllowingStateLoss
import com.google.android.material.textfield.TextInputLayout
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.text.DecimalFormat
import java.text.DecimalFormatSymbols

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
        private val channels2G = (1..14).map { ChannelOption(SoftApConfigurationCompat.BAND_2GHZ, it) }
        private val channels5G by lazy {
            channels2G + (1..196).map { ChannelOption(SoftApConfigurationCompat.BAND_5GHZ, it) }
        }

        private fun genAutoOptions(band: Int) = (1..band).filter { it and band == it }.map { ChannelOption(it) }
        /**
         * Source: https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/c2fc6a1/service/java/com/android/server/wifi/p2p/SupplicantP2pIfaceHal.java#1396
         */
        private val p2pUnsafeOptions by lazy {
            listOf(ChannelOption(SoftApConfigurationCompat.BAND_LEGACY)) +
                    channels2G + (15..165).map { ChannelOption(SoftApConfigurationCompat.BAND_5GHZ, it) }
        }
        private val p2pSafeOptions by lazy { genAutoOptions(SoftApConfigurationCompat.BAND_LEGACY) + channels5G }
        private val softApOptions by lazy {
            if (Build.VERSION.SDK_INT >= 30) {
                genAutoOptions(SoftApConfigurationCompat.BAND_ANY_31) +
                        channels5G +
                        (1..253).map { ChannelOption(SoftApConfigurationCompat.BAND_6GHZ, it) } +
                        (1..6).map { ChannelOption(SoftApConfigurationCompat.BAND_60GHZ, it) }
            } else p2pSafeOptions
        }

        @get:RequiresApi(30)
        private val bandWidthOptions by lazy {
            SoftApInfo.channelWidthLookup.lookup.let { lookup ->
                Array(lookup.size()) { BandWidth(lookup.keyAt(it), lookup.valueAt(it).substring(14)) }.apply { sort() }
            }
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

    private open class ChannelOption(val band: Int = 0, val channel: Int = 0) {
        object Disabled : ChannelOption(-1) {
            override fun toString() = app.getString(R.string.wifi_ap_choose_disabled)
        }
        override fun toString() = if (channel == 0) {
            val format = DecimalFormat("#.#", DecimalFormatSymbols.getInstance(app.resources.configuration.locales[0]))
            app.getString(R.string.wifi_ap_choose_G, arrayOf(
                SoftApConfigurationCompat.BAND_2GHZ to 2.4,
                SoftApConfigurationCompat.BAND_5GHZ to 5,
                SoftApConfigurationCompat.BAND_6GHZ to 6,
                SoftApConfigurationCompat.BAND_60GHZ to 60,
            ).filter { (mask, _) -> band and mask == mask }.joinToString("/") { (_, name) -> format.format(name) })
        } else "${SoftApConfigurationCompat.channelToFrequency(band, channel)} MHz ($channel)"
    }

    private class BandWidth(val width: Int, val name: String = "") : Comparable<BandWidth> {
        override fun compareTo(other: BandWidth) = width - other.width
        override fun toString() = name
    }

    private lateinit var dialogView: DialogWifiApBinding
    private lateinit var base: SoftApConfigurationCompat
    private var pasted = false
    private var started = false
    private val currentChannels get() = when {
        !arg.p2pMode -> softApOptions
        RepeaterService.safeMode -> p2pSafeOptions
        else -> p2pUnsafeOptions
    }
    private val acsList by lazy {
        listOf(
            Triple(SoftApConfigurationCompat.BAND_2GHZ, dialogView.acs2g, dialogView.acs2gWrapper),
            Triple(SoftApConfigurationCompat.BAND_5GHZ, dialogView.acs5g, dialogView.acs5gWrapper),
            Triple(SoftApConfigurationCompat.BAND_6GHZ, dialogView.acs6g, dialogView.acs6gWrapper),
        )
    }
    override val ret get() = Arg(generateConfig())
    private val hexToggleable get() = if (arg.p2pMode) !RepeaterService.safeMode else Build.VERSION.SDK_INT >= 33
    private var hexSsid = false
        set(value) {
            field = value
            dialogView.ssidWrapper.setEndIconActivated(value)
        }
    private val ssid get() = if (hexSsid) {
        WifiSsidCompat.fromHex(dialogView.ssid.text?.toString())
    } else WifiSsidCompat.fromUtf8Text(dialogView.ssid.text?.toString())

    private fun generateChannels() = SparseIntArray(2).apply {
        if (!arg.p2pMode && Build.VERSION.SDK_INT >= 31) {
            (dialogView.bandSecondary.selectedItem as ChannelOption?)?.apply { if (band >= 0) put(band, channel) }
        }
        (dialogView.bandPrimary.selectedItem as ChannelOption).apply { put(band, channel) }
    }
    private fun generateConfig(full: Boolean = true) = base.copy(
            ssid = ssid,
            passphrase = if (dialogView.password.length() != 0) dialogView.password.text.toString() else null).apply {
        if (!arg.p2pMode) {
            securityType = dialogView.security.selectedItemPosition
            isHiddenSsid = dialogView.hiddenSsid.isChecked
        }
        if (full) {
            isAutoShutdownEnabled = dialogView.autoShutdown.isChecked
            shutdownTimeoutMillis = dialogView.timeout.text.let { text ->
                if (text.isNullOrEmpty()) 0 else text.toString().toLong()
            }
            channels = generateChannels()
            maxNumberOfClients = dialogView.maxClient.text.let { text ->
                if (text.isNullOrEmpty()) 0 else text.toString().toInt()
            }
            isClientControlByUserEnabled = dialogView.clientUserControl.isChecked
            allowedClientList = (dialogView.allowedList.text ?: "").split(nonMacChars)
                    .filter { it.isNotEmpty() }.map(MacAddress::fromString)
            blockedClientList = (dialogView.blockedList.text ?: "").split(nonMacChars)
                    .filter { it.isNotEmpty() }.map(MacAddress::fromString)
            macRandomizationSetting = dialogView.macRandomization.selectedItemPosition
            bssid = if ((arg.p2pMode || Build.VERSION.SDK_INT < 31 && macRandomizationSetting ==
                        SoftApConfigurationCompat.RANDOMIZATION_NONE) && dialogView.bssid.length() != 0) {
                MacAddress.fromString(dialogView.bssid.text.toString())
            } else null
            isBridgedModeOpportunisticShutdownEnabled = dialogView.bridgedModeOpportunisticShutdown.isChecked
            isIeee80211axEnabled = dialogView.ieee80211ax.isChecked
            isIeee80211beEnabled = dialogView.ieee80211be.isChecked
            isUserConfiguration = dialogView.userConfig.isChecked
            bridgedModeOpportunisticShutdownTimeoutMillis = dialogView.bridgedTimeout.text.let { text ->
                if (text.isNullOrEmpty()) -1L else text.toString().toLong()
            }
            vendorElements = VendorElements.deserialize(dialogView.vendorElements.text)
            persistentRandomizedMacAddress = if (dialogView.persistentRandomizedMac.length() != 0) {
                MacAddress.fromString(dialogView.persistentRandomizedMac.text.toString())
            } else null
            allowedAcsChannels = acsList.associate { (band, text, _) -> band to RangeInput.fromString(text.text) }
            if (!arg.p2pMode && Build.VERSION.SDK_INT >= 33) {
                maxChannelBandwidth = (dialogView.maxChannelBandwidth.selectedItem as BandWidth).width
            }
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
        dialogView.ssidWrapper.setLengthCounter {
            try {
                ssid?.bytes?.size ?: 0
            } catch (_: IllegalArgumentException) {
                0
            }
        }
        if (hexToggleable) dialogView.ssidWrapper.apply {
            endIconMode = TextInputLayout.END_ICON_CUSTOM
            setEndIconOnClickListener {
                val ssid = try {
                    ssid
                } catch (_: IllegalArgumentException) {
                    return@setEndIconOnClickListener
                }
                val newText = if (hexSsid) ssid?.run {
                    decode().also { if (it == null) return@setEndIconOnClickListener }
                } else ssid?.hex
                hexSsid = !hexSsid
                dialogView.ssid.setText(newText)
            }
            findViewById<View>(com.google.android.material.R.id.text_input_end_icon).apply {
                tooltipText = contentDescription
            }
        }
        if (!arg.readOnly) dialogView.ssid.addTextChangedListener(this@WifiApDialogFragment)
        if (arg.p2pMode) dialogView.securityWrapper.isGone = true else dialogView.security.apply {
            adapter = ArrayAdapter(activity, android.R.layout.simple_spinner_item, 0,
                    SoftApConfigurationCompat.securityTypes).apply {
                setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
            }
            onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
                override fun onNothingSelected(parent: AdapterView<*>?) = error("Must select something")
                override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
                    when (position) {
                         SoftApConfiguration.SECURITY_TYPE_OPEN,
                         SoftApConfiguration.SECURITY_TYPE_WPA3_OWE_TRANSITION,
                         SoftApConfiguration.SECURITY_TYPE_WPA3_OWE -> dialogView.passwordWrapper.isGone = true
                         else -> {
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
                        }
                    }
                    validate()
                }
            }
        }
        if (!arg.readOnly) dialogView.password.addTextChangedListener(this@WifiApDialogFragment)
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
        dialogView.bandPrimary.configure(currentChannels)
        if (Build.VERSION.SDK_INT >= 31 && !arg.p2pMode) {
            dialogView.bandSecondary.configure(listOf(ChannelOption.Disabled) + currentChannels)
        } else dialogView.bandSecondary.isGone = true
        if (arg.p2pMode || Build.VERSION.SDK_INT < 30) dialogView.accessControlGroup.isGone = true
        else if (!arg.readOnly) {
            dialogView.maxClient.addTextChangedListener(this@WifiApDialogFragment)
            dialogView.blockedList.addTextChangedListener(this@WifiApDialogFragment)
            dialogView.allowedList.addTextChangedListener(this@WifiApDialogFragment)
        }
        if (!arg.readOnly) dialogView.bssid.addTextChangedListener(this@WifiApDialogFragment)
        if (arg.p2pMode) dialogView.hiddenSsid.isGone = true
        if (arg.p2pMode && Build.VERSION.SDK_INT >= 29) dialogView.macRandomization.isEnabled = false
        else if (arg.p2pMode || Build.VERSION.SDK_INT < 31) dialogView.macRandomizationWrapper.isGone = true
        else dialogView.macRandomization.onItemSelectedListener = this@WifiApDialogFragment
        if (arg.p2pMode || Build.VERSION.SDK_INT < 31) {
            dialogView.ieee80211ax.isGone = true
            dialogView.bridgedModeOpportunisticShutdown.isGone = true
            dialogView.userConfig.isGone = true
            dialogView.bridgedTimeoutWrapper.isGone = true
        } else {
            dialogView.bridgedTimeoutWrapper.helperText = getString(R.string.wifi_hotspot_timeout_default,
                TetherTimeoutMonitor.defaultTimeoutBridged)
        }
        if (Build.VERSION.SDK_INT < 33) dialogView.vendorElementsWrapper.isGone = true
        else if (!arg.readOnly) dialogView.vendorElements.addTextChangedListener(this@WifiApDialogFragment)
        if (arg.p2pMode || Build.VERSION.SDK_INT < 33) {
            dialogView.ieee80211be.isGone = true
            dialogView.bridgedTimeout.isEnabled = false
            dialogView.persistentRandomizedMacWrapper.isGone = true
            for ((_, _, wrapper) in acsList) wrapper.isGone = true
            dialogView.maxChannelBandwidthWrapper.isGone = true
        } else {
            dialogView.maxChannelBandwidth.adapter = ArrayAdapter(activity, android.R.layout.simple_spinner_item, 0,
                bandWidthOptions).apply {
                setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
            }
            if (!arg.readOnly) {
                dialogView.bridgedTimeout.addTextChangedListener(this@WifiApDialogFragment)
                dialogView.persistentRandomizedMac.addTextChangedListener(this@WifiApDialogFragment)
                for ((_, text, _) in acsList) text.addTextChangedListener(this@WifiApDialogFragment)
                dialogView.acs5g.addTextChangedListener(this@WifiApDialogFragment)
                dialogView.acs6g.addTextChangedListener(this@WifiApDialogFragment)
                dialogView.maxChannelBandwidth.onItemSelectedListener = this@WifiApDialogFragment
            }
        }
        base = arg.configuration
        populateFromConfiguration()
    }

    private fun locate(i: Int): Int {
        val band = base.channels.keyAt(i)
        val channel = base.channels.valueAt(i)
        val selection = currentChannels.indexOfFirst { it.band == band && it.channel == channel }
        return if (selection == -1) {
            val msg = "Unable to locate $band, $channel, ${arg.p2pMode && !RepeaterService.safeMode}"
            if (pasted || arg.p2pMode) Timber.w(msg) else Timber.w(Exception(msg))
            0
        } else selection
    }
    private fun populateFromConfiguration() {
        dialogView.ssid.setText(base.ssid.let { ssid ->
            when {
                ssid == null -> null
                hexSsid -> ssid.hex
                hexToggleable -> ssid.decode() ?: ssid.hex.also { hexSsid = true }
                else -> ssid.toString()
            }
        })
        if (!arg.p2pMode) dialogView.security.setSelection(base.securityType)
        dialogView.password.setText(base.passphrase)
        dialogView.autoShutdown.isChecked = base.isAutoShutdownEnabled
        dialogView.timeout.setText(base.shutdownTimeoutMillis.let { if (it <= 0) "" else it.toString() })
        dialogView.bandPrimary.setSelection(locate(0))
        if (Build.VERSION.SDK_INT >= 31 && !arg.p2pMode) {
            dialogView.bandSecondary.setSelection(if (base.channels.size() > 1) locate(1) + 1 else 0)
        }
        dialogView.bssid.setText(base.bssid?.toString())
        dialogView.hiddenSsid.isChecked = base.isHiddenSsid
        dialogView.maxClient.setText(base.maxNumberOfClients.let { if (it == 0) "" else it.toString() })
        dialogView.clientUserControl.isChecked = base.isClientControlByUserEnabled
        dialogView.blockedList.setText(base.blockedClientList.joinToString("\n"))
        dialogView.allowedList.setText(base.allowedClientList.joinToString("\n"))
        dialogView.macRandomization.setSelection(base.macRandomizationSetting)
        dialogView.bridgedModeOpportunisticShutdown.isChecked = base.isBridgedModeOpportunisticShutdownEnabled
        dialogView.ieee80211ax.isChecked = base.isIeee80211axEnabled
        dialogView.ieee80211be.isChecked = base.isIeee80211beEnabled
        dialogView.userConfig.isChecked = base.isUserConfiguration
        dialogView.bridgedTimeout.setText(base.bridgedModeOpportunisticShutdownTimeoutMillis.let {
            if (it == -1L) "" else it.toString()
        })
        dialogView.vendorElements.setText(VendorElements.serialize(base.vendorElements))
        dialogView.persistentRandomizedMac.setText(base.persistentRandomizedMacAddress?.toString())
        for ((band, text, _) in acsList) text.setText(RangeInput.toString(base.allowedAcsChannels[band]))
        if (Build.VERSION.SDK_INT >= 33) bandWidthOptions.binarySearch(BandWidth(base.maxChannelBandwidth)).let {
            if (it < 0) {
                Timber.w(Exception("Cannot locate bandwidth ${base.maxChannelBandwidth}"))
            } else dialogView.maxChannelBandwidth.setSelection(it)
        }
    }

    override fun onStart() {
        super.onStart()
        started = true
        validate()
    }

    private fun validate() {
        if (!started) return
        val (ssidOk, ssidError) = 0.let {
            val ssid = try {
                ssid
            } catch (e: IllegalArgumentException) {
                return@let false to e.readableMessage
            }
            val ssidLength = ssid?.bytes?.size ?: 0
            if (ssidLength in 1..32) true to if (arg.p2pMode && RepeaterService.safeMode && ssidLength < 9) {
                requireContext().getString(R.string.settings_service_repeater_safe_mode_warning)
            } else null else false to " "
        }
        dialogView.ssidWrapper.error = ssidError
        val selectedSecurity = if (arg.p2pMode) {
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
        } else dialogView.security.selectedItemPosition
        // see also: https://android.googlesource.com/platform/frameworks/base/+/92c8f59/wifi/java/android/net/wifi/SoftApConfiguration.java#688
        val passwordValid = when (selectedSecurity) {
            SoftApConfiguration.SECURITY_TYPE_OPEN,
            SoftApConfiguration.SECURITY_TYPE_WPA3_OWE_TRANSITION,
            SoftApConfiguration.SECURITY_TYPE_WPA3_OWE -> true
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK, SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION -> {
                dialogView.password.length() in 8..63
            }
            else -> dialogView.password.length() > 0
        }
        dialogView.passwordWrapper.error = if (passwordValid) null else " "
        val timeoutError = dialogView.timeout.text.let { text ->
            if (text.isNullOrEmpty()) null else try {
                SoftApConfigurationCompat.testPlatformTimeoutValidity(text.toString().toLong())
                null
            } catch (e: Exception) {
                e.readableMessage
            }
        }
        dialogView.timeoutWrapper.error = timeoutError
        val bandError = if (!arg.p2pMode && Build.VERSION.SDK_INT >= 30) {
            try {
                SoftApConfigurationCompat.testPlatformValidity(generateChannels())
                null
            } catch (e: Exception) {
                e.readableMessage
            }
        } else null
        dialogView.bandError.isGone = bandError.isNullOrEmpty()
        dialogView.bandError.text = bandError
        val hideBssid = !arg.p2pMode && Build.VERSION.SDK_INT >= 31 &&
                dialogView.macRandomization.selectedItemPosition != SoftApConfigurationCompat.RANDOMIZATION_NONE
        dialogView.bssidWrapper.isGone = hideBssid
        dialogView.bssidWrapper.error = null
        val bssidValid = hideBssid || dialogView.bssid.length() == 0 || try {
            val mac = MacAddress.fromString(dialogView.bssid.text.toString())
            if (Build.VERSION.SDK_INT >= 30 && !arg.p2pMode) SoftApConfigurationCompat.testPlatformValidity(mac)
            true
        } catch (e: Exception) {
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
                (dialogView.blockedList.text ?: "").split(nonMacChars).filter { it.isNotEmpty() }
                    .map(MacAddress::fromString).toSet() to null
            } catch (e: IllegalArgumentException) {
                null to e.readableMessage
            }
            dialogView.blockedListWrapper.error = blockedListError
            val allowedListError = try {
                (dialogView.allowedList.text ?: "").split(nonMacChars).filter { it.isNotEmpty() }.forEach {
                    val mac = MacAddress.fromString(it)
                    require(blockedList?.contains(mac) != true) { "A MAC address exists in both client lists" }
                }
                null
            } catch (e: IllegalArgumentException) {
                e.readableMessage
            }
            dialogView.allowedListWrapper.error = allowedListError
            blockedListError == null && allowedListError == null
        } else true
        val bridgedTimeoutError = dialogView.bridgedTimeout.text.let { text ->
            if (text.isNullOrEmpty()) null else try {
                SoftApConfigurationCompat.testPlatformBridgedTimeoutValidity(text.toString().toLong())
                null
            } catch (e: Exception) {
                e.readableMessage
            }
        }
        dialogView.bridgedTimeoutWrapper.error = bridgedTimeoutError
        val vendorElementsError = if (Build.VERSION.SDK_INT >= 33) {
            try {
                VendorElements.deserialize(dialogView.vendorElements.text).also {
                    if (!arg.p2pMode) SoftApConfigurationCompat.testPlatformValidity(it)
                }
                null
            } catch (e: Exception) {
                e.readableMessage
            }
        } else null
        dialogView.vendorElementsWrapper.error = vendorElementsError
        dialogView.persistentRandomizedMacWrapper.error = null
        val persistentRandomizedMacValid = dialogView.persistentRandomizedMac.length() == 0 || try {
            MacAddress.fromString(dialogView.persistentRandomizedMac.text.toString())
            true
        } catch (e: IllegalArgumentException) {
            dialogView.persistentRandomizedMacWrapper.error = e.readableMessage
            false
        }
        val acsNoError = if (!arg.p2pMode && Build.VERSION.SDK_INT >= 33) acsList.all { (band, text, wrapper) ->
            try {
                wrapper.error = null
                SoftApConfigurationCompat.testPlatformValidity(band, RangeInput.fromString(text.text).toIntArray())
                true
            } catch (e: Exception) {
                wrapper.error = e.readableMessage
                false
            }
        } else true
        val bandwidthError = if (!arg.p2pMode && Build.VERSION.SDK_INT >= 33) {
            try {
                SoftApConfigurationCompat.testPlatformValidity(
                    (dialogView.maxChannelBandwidth.selectedItem as BandWidth).width)
                null
            } catch (e: Exception) {
                e.readableMessage
            }
        } else null
        dialogView.maxChannelBandwidthError.isGone = bandwidthError.isNullOrEmpty()
        dialogView.maxChannelBandwidthError.text = bandwidthError
        val canCopy = ssidOk && timeoutError == null && bssidValid && maxClientError == null && listsNoError &&
                bridgedTimeoutError == null && vendorElementsError == null && persistentRandomizedMacValid &&
                acsNoError && bandwidthError == null
        val canGenerate = canCopy && passwordValid && bandError == null
        (dialog as? AlertDialog)?.getButton(DialogInterface.BUTTON_POSITIVE)?.isEnabled = canGenerate
        dialogView.toolbar.menu.apply {
            findItem(R.id.invalid).isVisible = canGenerate && Build.VERSION.SDK_INT >= 34 && !arg.p2pMode &&
                    !arg.readOnly && !Services.wifi.validateSoftApConfiguration(generateConfig().toPlatform())
            findItem(android.R.id.copy).isEnabled = canCopy
            findItem(R.id.share_qr).isEnabled = ssidOk
        }
    }

    override fun onTextChanged(s: CharSequence, start: Int, before: Int, count: Int) { }
    override fun beforeTextChanged(s: CharSequence, start: Int, count: Int, after: Int) { }
    override fun afterTextChanged(editable: Editable) = validate()

    override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) = validate()
    override fun onNothingSelected(parent: AdapterView<*>?) = error("unreachable")

    override fun onMenuItemClick(item: MenuItem?): Boolean {
        return when (item?.itemId) {
            android.R.id.copy -> try {
                app.clipboard.setPrimaryClip(ClipData.newPlainText(null,
                        Base64.encodeToString(generateConfig().toByteArray(), BASE64_FLAGS)).apply {
                    description.extras = persistableBundleOf(ClipDescription.EXTRA_IS_SENSITIVE to true)
                })
                true
            } catch (e: RuntimeException) {
                Toast.makeText(context, e.readableMessage, Toast.LENGTH_LONG).show()
                false
            }
            android.R.id.paste -> try {
                app.clipboard.primaryClip?.getItemAt(0)?.text?.apply {
                    Base64.decode(toString(), BASE64_FLAGS).toParcelable<SoftApConfigurationCompat>()?.let { config ->
                        val newUnderlying = config.underlying
                        if (newUnderlying != null) {
                            arg.configuration.underlying?.let { check(it.javaClass == newUnderlying.javaClass) }
                        } else config.underlying = arg.configuration.underlying
                        base = config
                        pasted = true
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
