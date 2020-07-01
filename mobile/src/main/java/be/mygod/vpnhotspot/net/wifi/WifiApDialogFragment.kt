package be.mygod.vpnhotspot.net.wifi

import android.annotation.SuppressLint
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
        private val channels by lazy { (1..165).map { BandOption.Channel(it) } }
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
        class Channel(override val channel: Int) : BandOption() {
            override fun toString() = "${SoftApConfigurationCompat.channelToFrequency(channel)} MHz ($channel)"
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
        if (full) {
            val bandOption = dialogView.band.selectedItem as BandOption
            band = bandOption.band
            channel = bandOption.channel
            bssid = if (dialogView.bssid.length() != 0) {
                MacAddressCompat.fromString(dialogView.bssid.text.toString())
            } else null
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
        if (Build.VERSION.SDK_INT >= 23 || arg.p2pMode) dialogView.band.apply {
            bandOptions = mutableListOf<BandOption>().apply {
                if (arg.p2pMode) {
                    add(BandOption.BandAny)
                    if (RepeaterService.safeMode) {
                        add(BandOption.Band2GHz)
                        add(BandOption.Band5GHz)
                    }
                } else {
                    if (Build.VERSION.SDK_INT >= 28) add(BandOption.BandAny)
                    add(BandOption.Band2GHz)
                    add(BandOption.Band5GHz)
                    if (Build.VERSION.SDK_INT >= 30) add(BandOption.Band6GHz)
                }
                addAll(channels)
            }
            adapter = ArrayAdapter(activity, android.R.layout.simple_spinner_item, 0, bandOptions).apply {
                setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
            }
        } else dialogView.bandWrapper.isGone = true
        dialogView.bssid.addTextChangedListener(this@WifiApDialogFragment)
        if (arg.p2pMode) dialogView.hiddenSsid.isGone = true
        base = arg.configuration
        populateFromConfiguration()
    }

    private fun populateFromConfiguration() {
        dialogView.ssid.setText(base.ssid)
        if (!arg.p2pMode) dialogView.security.setSelection(base.securityType)
        dialogView.password.setText(base.passphrase)
        if (Build.VERSION.SDK_INT >= 23 || arg.p2pMode) {
            dialogView.band.setSelection(if (base.channel in 1..165) {
                bandOptions.indexOfFirst { it.channel == base.channel }
            } else bandOptions.indexOfFirst { it.band == base.band })
        }
        dialogView.bssid.setText(base.bssid?.toString())
        dialogView.hiddenSsid.isChecked = base.isHiddenSsid
        // TODO support more fields from SACC
    }

    override fun onStart() {
        super.onStart()
        started = true
        if (!arg.readOnly) validate()
    }

    /**
     * This function is reached only if not arg.readOnly.
     */
    private fun validate() {
        if (!started) return
        val ssidLength = dialogView.ssid.text.toString().toByteArray().size
        dialogView.ssidWrapper.error = if (arg.p2pMode && RepeaterService.safeMode && ssidLength < 9) {
            requireContext().getString(R.string.settings_service_repeater_safe_mode_warning)
        } else null
        val selectedSecurity = if (arg.p2pMode) {
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
        } else dialogView.security.selectedItemPosition
        val passwordValid = when (selectedSecurity) {
            // TODO
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK -> dialogView.password.length() >= 8
            else -> true    // do not try to validate
        }
        dialogView.passwordWrapper.error = if (passwordValid) null else {
            requireContext().getString(R.string.credentials_password_too_short)
        }
        dialogView.bssidWrapper.error = null
        val bssidValid = dialogView.bssid.length() == 0 || try {
            MacAddressCompat.fromString(dialogView.bssid.text.toString())
            true
        } catch (e: IllegalArgumentException) {
            dialogView.bssidWrapper.error = e.readableMessage
            false
        }
        (dialog as? AlertDialog)?.getButton(DialogInterface.BUTTON_POSITIVE)?.isEnabled =
                ssidLength in 1..32 && passwordValid && bssidValid
        dialogView.toolbar.menu.findItem(android.R.id.copy).isEnabled = bssidValid
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
