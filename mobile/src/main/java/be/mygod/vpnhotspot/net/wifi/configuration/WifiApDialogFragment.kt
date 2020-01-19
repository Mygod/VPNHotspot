package be.mygod.vpnhotspot.net.wifi.configuration

import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.content.ClipData
import android.content.DialogInterface
import android.net.wifi.WifiConfiguration
import android.os.Build
import android.os.Parcelable
import android.text.Editable
import android.text.TextWatcher
import android.util.Base64
import android.view.MenuItem
import android.view.View
import android.widget.AdapterView
import android.widget.ArrayAdapter
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.widget.Toolbar
import androidx.core.view.isGone
import be.mygod.vpnhotspot.AlertDialogFragment
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.databinding.DialogWifiApBinding
import be.mygod.vpnhotspot.util.QRCodeDialog
import be.mygod.vpnhotspot.util.toByteArray
import be.mygod.vpnhotspot.util.toParcelable
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.android.parcel.Parcelize
import java.nio.charset.Charset

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
    data class Arg(val configuration: WifiConfiguration,
                   val readOnly: Boolean = false,
                   /**
                    * KeyMgmt is enforced to WPA_PSK.
                    * Various values for apBand are allowed according to different rules.
                    */
                   val p2pMode: Boolean = false) : Parcelable

    @TargetApi(23)
    private sealed class BandOption {
        open val apBand get() = AP_BAND_2GHZ
        open val apChannel get() = 0

        object BandAny : BandOption() {
            override val apBand get() = AP_BAND_ANY
            override fun toString() = app.getString(R.string.wifi_ap_choose_auto)
        }
        object Band2GHz : BandOption() {
            override fun toString() = app.getString(R.string.wifi_ap_choose_2G)
        }
        object Band5GHz : BandOption() {
            override val apBand get() = AP_BAND_5GHZ
            override fun toString() = app.getString(R.string.wifi_ap_choose_5G)
        }
        class Channel(override val apChannel: Int) : BandOption() {
            override fun toString() = "${channelToFrequency(apChannel)} MHz ($apChannel)"
        }
    }

    private lateinit var dialogView: DialogWifiApBinding
    private lateinit var bandOptions: MutableList<BandOption>
    private var started = false
    private val selectedSecurity get() =
        if (arg.p2pMode) WifiConfiguration.KeyMgmt.WPA_PSK else dialogView.security.selectedItemPosition
    override val ret get() = Arg(WifiConfiguration().apply {
        SSID = dialogView.ssid.text.toString()
        allowedKeyManagement.set(selectedSecurity)
        allowedAuthAlgorithms.set(WifiConfiguration.AuthAlgorithm.OPEN)
        if (dialogView.password.length() != 0) preSharedKey = dialogView.password.text.toString()
        if (Build.VERSION.SDK_INT >= 23) {
            val bandOption = dialogView.band.selectedItem as BandOption
            apBand = bandOption.apBand
            apChannel = bandOption.apChannel
        }
    })

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
                    WifiConfiguration.KeyMgmt.strings).apply {
                setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
            }
            onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
                override fun onNothingSelected(parent: AdapterView<*>?) = error("Must select something")
                override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
                    dialogView.passwordWrapper.isGone = position == WifiConfiguration.KeyMgmt.NONE
                }
            }
        }
        if (!arg.readOnly) dialogView.password.addTextChangedListener(this@WifiApDialogFragment)
        if (Build.VERSION.SDK_INT >= 23 || arg.p2pMode) dialogView.band.apply {
            bandOptions = mutableListOf<BandOption>().apply {
                if (arg.p2pMode) {
                    add(BandOption.BandAny)
                    if (Build.VERSION.SDK_INT >= 29) {
                        add(BandOption.Band2GHz)
                        add(BandOption.Band5GHz)
                    }
                } else {
                    if (Build.VERSION.SDK_INT >= 28) add(BandOption.BandAny)
                    add(BandOption.Band2GHz)
                    add(BandOption.Band5GHz)
                }
                addAll(channels)
            }
            adapter = ArrayAdapter(activity, android.R.layout.simple_spinner_item, 0, bandOptions).apply {
                setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
            }
            if (Build.VERSION.SDK_INT < 23) {
                setSelection(bandOptions.indexOfFirst { it.apChannel == RepeaterService.operatingChannel })
            }
        } else dialogView.bandWrapper.isGone = true
        populateFromConfiguration(arg.configuration)
    }

    private fun populateFromConfiguration(configuration: WifiConfiguration) {
        dialogView.ssid.setText(configuration.SSID)
        if (!arg.p2pMode) dialogView.security.setSelection(configuration.apKeyManagement)
        dialogView.password.setText(configuration.preSharedKey)
        if (Build.VERSION.SDK_INT >= 23) {
            dialogView.band.setSelection(if (configuration.apChannel in 1..165) {
                bandOptions.indexOfFirst { it.apChannel == configuration.apChannel }
            } else bandOptions.indexOfFirst { it.apBand == configuration.apBand })
        }
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
        val ssidValid = dialogView.ssid.length() != 0 &&
                Charset.forName("UTF-8").encode(dialogView.ssid.text.toString()).limit() <= 32
        val passwordValid = when (selectedSecurity) {
            WifiConfiguration.KeyMgmt.WPA_PSK, WPA2_PSK -> dialogView.password.length() >= 8
            else -> true    // do not try to validate
        }
        dialogView.passwordWrapper.error = if (passwordValid) null else {
            requireContext().getString(R.string.credentials_password_too_short)
        }
        (dialog as? AlertDialog)?.getButton(DialogInterface.BUTTON_POSITIVE)?.isEnabled = ssidValid && passwordValid
    }

    override fun onTextChanged(s: CharSequence, start: Int, before: Int, count: Int) { }
    override fun beforeTextChanged(s: CharSequence, start: Int, count: Int, after: Int) { }
    override fun afterTextChanged(editable: Editable) = validate()

    override fun onMenuItemClick(item: MenuItem?): Boolean {
        return when (item?.itemId) {
            android.R.id.copy -> {
                app.clipboard.setPrimaryClip(ClipData.newPlainText(null,
                        Base64.encodeToString(ret.configuration.toByteArray(), BASE64_FLAGS)))
                true
            }
            android.R.id.paste -> try {
                app.clipboard.primaryClip?.getItemAt(0)?.text?.apply {
                    Base64.decode(toString(), BASE64_FLAGS).toParcelable<WifiConfiguration>()
                            ?.let { populateFromConfiguration(it) }
                }
                true
            } catch (e: IllegalArgumentException) {
                SmartSnackbar.make(e).show()
                false
            }
            R.id.share_qr -> {
                val qrString = try {
                    ret.configuration.toQRString()
                } catch (e: IllegalArgumentException) {
                    SmartSnackbar.make(e).show()
                    return false
                }
                QRCodeDialog().withArg(qrString).show(parentFragmentManager, "QRCodeDialog")
                true
            }
            else -> false
        }
    }

    override fun onClick(dialog: DialogInterface?, which: Int) {
        super.onClick(dialog, which)
        if (Build.VERSION.SDK_INT < 23 && arg.p2pMode && which == DialogInterface.BUTTON_POSITIVE) {
            RepeaterService.operatingChannel = (dialogView.band.selectedItem as BandOption).apChannel
        }
    }
}
