package be.mygod.vpnhotspot.net.wifi

import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.content.ClipData
import android.content.DialogInterface
import android.net.wifi.SoftApConfiguration
import android.os.Build
import android.os.Parcelable
import android.text.Editable
import android.text.TextWatcher
import android.util.Base64
import android.view.MenuItem
import android.view.View
import android.widget.AdapterView
import android.widget.ArrayAdapter
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
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.android.parcel.Parcelize

/**
 * Based on: https://android.googlesource.com/platform/packages/apps/Settings/+/39b4674/src/com/android/settings/wifi/WifiApDialog.java
 *
 * This dialog has been deprecated in API 28, but we are still using it since it works better for our purposes.
 * Related: https://android.googlesource.com/platform/packages/apps/Settings/+/defb1183ecb00d6231bac7d934d07f58f90261ea
 */
class WifiApDialogFragment : AlertDialogFragment<WifiApDialogFragment.Arg, WifiApDialogFragment.Arg>(), TextWatcher,
        Toolbar.OnMenuItemClickListener {
    companion object {
        private const val BASE64_FLAGS = Base64.NO_PADDING or Base64.NO_WRAP
        private val nonMacChars = "[^0-9a-fA-F:]+".toRegex()
        private val channels by lazy {
            val list = ArrayList<BandOption.Channel>()
            for (chan in 1..14) list.add(BandOption.Channel(SoftApConfigurationCompat.BAND_2GHZ, chan))
            for (chan in 1..196) list.add(BandOption.Channel(SoftApConfigurationCompat.BAND_5GHZ, chan))
            if (Build.VERSION.SDK_INT >= 30) {
                for (chan in 1..253) list.add(BandOption.Channel(SoftApConfigurationCompat.BAND_6GHZ, chan))
            }
            list
        }
        /**
         * Source: https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/c2fc6a1/service/java/com/android/server/wifi/p2p/SupplicantP2pIfaceHal.java#1396
         */
        private val p2pChannels by lazy {
            (1..165).map {
                val band = if (it <= 14) SoftApConfigurationCompat.BAND_2GHZ else SoftApConfigurationCompat.BAND_5GHZ
                BandOption.Channel(band, it)
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

    private sealed class BandOption {
        open val band get() = SoftApConfigurationCompat.BAND_ANY
        open val channel get() = 0

        object BandAny : BandOption() {
            override fun toString() = app.getString(R.string.wifi_ap_choose_auto)
        }
        object Band2GHz : BandOption() {
            override val band get() = SoftApConfigurationCompat.BAND_2GHZ
            override fun toString() = app.getString(R.string.wifi_ap_choose_2G)
        }
        object Band5GHz : BandOption() {
            override val band get() = SoftApConfigurationCompat.BAND_5GHZ
            override fun toString() = app.getString(R.string.wifi_ap_choose_5G)
        }
        @RequiresApi(30)
        object Band6GHz : BandOption() {
            override val band get() = SoftApConfigurationCompat.BAND_6GHZ
            override fun toString() = app.getString(R.string.wifi_ap_choose_6G)
        }
        class Channel(override val band: Int, override val channel: Int) : BandOption() {
            override fun toString() = "${SoftApConfigurationCompat.channelToFrequency(band, channel)} MHz ($channel)"
        }
    }

    private lateinit var dialogView: DialogWifiApBinding
    private lateinit var bandOptions: MutableList<BandOption>
    private lateinit var base: SoftApConfigurationCompat
    private var started = false
    override val ret get() = Arg(generateConfig())

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
            if (Build.VERSION.SDK_INT >= 23 || arg.p2pMode) {
                val bandOption = dialogView.band.selectedItem as BandOption
                band = bandOption.band
                channel = bandOption.channel
            }
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
                    dialogView.passwordWrapper.isGone = position == SoftApConfiguration.SECURITY_TYPE_OPEN
                }
            }
        }
        if (!arg.readOnly) dialogView.password.addTextChangedListener(this@WifiApDialogFragment)
        if (!arg.p2pMode && Build.VERSION.SDK_INT < 28) dialogView.autoShutdown.isGone = true
        if (arg.p2pMode || Build.VERSION.SDK_INT >= 30) {
            dialogView.timeoutWrapper.helperText = getString(R.string.wifi_hotspot_timeout_default,
                    TetherTimeoutMonitor.defaultTimeout)
            dialogView.timeout.addTextChangedListener(this@WifiApDialogFragment)
        } else dialogView.timeoutWrapper.isGone = true
        if (Build.VERSION.SDK_INT >= 23 || arg.p2pMode) dialogView.band.apply {
            bandOptions = mutableListOf<BandOption>().apply {
                if (arg.p2pMode) {
                    add(BandOption.BandAny)
                    if (RepeaterService.safeMode) {
                        add(BandOption.Band2GHz)
                        add(BandOption.Band5GHz)
                        addAll(channels)
                    } else addAll(p2pChannels)
                } else {
                    if (Build.VERSION.SDK_INT >= 28) add(BandOption.BandAny)
                    add(BandOption.Band2GHz)
                    add(BandOption.Band5GHz)
                    if (Build.VERSION.SDK_INT >= 30) add(BandOption.Band6GHz)
                    addAll(channels)
                }
            }
            adapter = ArrayAdapter(activity, android.R.layout.simple_spinner_item, 0, bandOptions).apply {
                setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
            }
        } else dialogView.bandWrapper.isGone = true
        dialogView.bssid.addTextChangedListener(this@WifiApDialogFragment)
        if (arg.p2pMode) dialogView.hiddenSsid.isGone = true
        if (arg.p2pMode || Build.VERSION.SDK_INT < 30) {
            dialogView.maxClientWrapper.isGone = true
            dialogView.clientUserControl.isGone = true
            dialogView.blockedListWrapper.isGone = true
            dialogView.allowedListWrapper.isGone = true
        } else {
            dialogView.maxClient.addTextChangedListener(this@WifiApDialogFragment)
            dialogView.blockedList.addTextChangedListener(this@WifiApDialogFragment)
            dialogView.allowedList.addTextChangedListener(this@WifiApDialogFragment)
        }
        base = arg.configuration
        populateFromConfiguration()
    }

    private fun populateFromConfiguration() {
        dialogView.ssid.setText(base.ssid)
        if (!arg.p2pMode) dialogView.security.setSelection(base.securityType)
        dialogView.password.setText(base.passphrase)
        dialogView.autoShutdown.isChecked = base.isAutoShutdownEnabled
        dialogView.timeout.setText(base.shutdownTimeoutMillis.let { if (it == 0L) "" else it.toString() })
        if (Build.VERSION.SDK_INT >= 23 || arg.p2pMode) {
            val selection = if (base.channel != 0) {
                bandOptions.indexOfFirst { it.channel == base.channel }
            } else bandOptions.indexOfFirst { it.band == base.band }
            dialogView.band.setSelection(if (selection == -1) 0 else selection)
        }
        dialogView.bssid.setText(base.bssid?.toString())
        dialogView.hiddenSsid.isChecked = base.isHiddenSsid
        dialogView.maxClient.setText(base.maxNumberOfClients.let { if (it == 0) "" else it.toString() })
        dialogView.clientUserControl.isChecked = base.isClientControlByUserEnabled
        dialogView.blockedList.setText(base.blockedClientList.joinToString("\n"))
        dialogView.allowedList.setText(base.allowedClientList.joinToString("\n"))
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
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK, SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION -> {
                dialogView.password.length() >= 8
            }
            else -> true    // do not try to validate
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
        val blockedListError = try {
            (dialogView.blockedList.text ?: "").split(nonMacChars)
                    .filter { it.isNotEmpty() }.forEach { MacAddressCompat.fromString(it).toPlatform() }
            null
        } catch (e: IllegalArgumentException) {
            e.readableMessage
        }
        dialogView.blockedListWrapper.error = blockedListError
        val allowedListError = try {
            (dialogView.allowedList.text ?: "").split(nonMacChars)
                    .filter { it.isNotEmpty() }.forEach { MacAddressCompat.fromString(it).toPlatform() }
            null
        } catch (e: IllegalArgumentException) {
            e.readableMessage
        }
        dialogView.allowedListWrapper.error = allowedListError
        val canCopy = timeoutError == null && bssidValid && maxClientError == null && blockedListError == null &&
                allowedListError == null
        (dialog as? AlertDialog)?.getButton(DialogInterface.BUTTON_POSITIVE)?.isEnabled =
                ssidLength in 1..32 && passwordValid && canCopy
        dialogView.toolbar.menu.findItem(android.R.id.copy).isEnabled = canCopy
    }

    override fun onTextChanged(s: CharSequence, start: Int, before: Int, count: Int) { }
    override fun beforeTextChanged(s: CharSequence, start: Int, count: Int, after: Int) { }
    override fun afterTextChanged(editable: Editable) = validate()

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
            } catch (e: IllegalArgumentException) {
                SmartSnackbar.make(e).show()
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
