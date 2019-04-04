package be.mygod.vpnhotspot.net.wifi.configuration

import android.annotation.TargetApi
import android.content.DialogInterface
import android.net.wifi.WifiConfiguration
import android.net.wifi.WifiConfiguration.AuthAlgorithm
import android.os.Build
import android.os.Parcelable
import android.text.Editable
import android.text.TextWatcher
import android.view.View
import android.widget.AdapterView
import android.widget.ArrayAdapter
import androidx.appcompat.app.AlertDialog
import androidx.core.view.isGone
import be.mygod.vpnhotspot.AlertDialogFragment
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import kotlinx.android.parcel.Parcelize
import kotlinx.android.synthetic.main.dialog_wifi_ap.view.*
import java.lang.IllegalStateException
import java.nio.charset.Charset

/**
 * Based on: https://android.googlesource.com/platform/packages/apps/Settings/+/39b4674/src/com/android/settings/wifi/WifiApDialog.java
 *
 * This dialog has been deprecated in API 28, but we are still using it since it works better for our purposes.
 * Related: https://android.googlesource.com/platform/packages/apps/Settings/+/defb1183ecb00d6231bac7d934d07f58f90261ea
 */
class WifiApDialogFragment : AlertDialogFragment<WifiApDialogFragment.Arg, WifiApDialogFragment.Arg>(), TextWatcher {
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

    private lateinit var dialogView: View
    override val ret: Arg? get() {
        return Arg(WifiConfiguration().apply {
            SSID = dialogView.ssid.text.toString()
            allowedKeyManagement.set(
                    if (arg.p2pMode) WifiConfiguration.KeyMgmt.WPA_PSK else dialogView.security.selectedItemPosition)
            allowedAuthAlgorithms.set(AuthAlgorithm.OPEN)
            if (dialogView.password.length() != 0) preSharedKey = dialogView.password.text.toString()
            if (Build.VERSION.SDK_INT >= 23) {
                val bandOption = dialogView.band.selectedItem as BandOption
                apBand = bandOption.apBand
                apChannel = bandOption.apChannel
            }
        })
    }

    override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
        val activity = requireActivity()
        dialogView = activity.layoutInflater.inflate(R.layout.dialog_wifi_ap, null)
        setView(dialogView)
        setTitle(R.string.configuration_view)
        if (!arg.readOnly) setPositiveButton(R.string.wifi_save, listener)
        setNegativeButton(R.string.donations__button_close, null)
        dialogView.ssid.setText(arg.configuration.SSID)
        if (!arg.readOnly) dialogView.ssid.addTextChangedListener(this@WifiApDialogFragment)
        if (arg.p2pMode) dialogView.security_wrapper.isGone = true else dialogView.security.apply {
            adapter = ArrayAdapter(activity, android.R.layout.simple_spinner_item, 0,
                    WifiConfiguration.KeyMgmt.strings).apply {
                setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
            }
            onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
                override fun onNothingSelected(parent: AdapterView<*>?) =
                        throw IllegalStateException("Must select something")
                override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
                    dialogView.password_wrapper.isGone = position == WifiConfiguration.KeyMgmt.NONE
                }
            }
            val selected = arg.configuration.allowedKeyManagement.nextSetBit(0)
            check(selected >= 0) { "No key management selected" }
            check(arg.configuration.allowedKeyManagement.nextSetBit(selected + 1) < 0) {
                "More than 1 key managements supplied"
            }
            setSelection(selected)
        }
        dialogView.password.setText(arg.configuration.preSharedKey)
        if (!arg.readOnly) dialogView.password.addTextChangedListener(this@WifiApDialogFragment)
        if (Build.VERSION.SDK_INT >= 23) dialogView.band.apply {
            val options = mutableListOf<BandOption>().apply {
                if (arg.p2pMode) add(BandOption.BandAny) else {
                    if (Build.VERSION.SDK_INT >= 28) add(BandOption.BandAny)
                    add(BandOption.Band2GHz)
                    add(BandOption.Band5GHz)
                }
                addAll((1..165).map { BandOption.Channel(it) })
            }
            adapter = ArrayAdapter(activity, android.R.layout.simple_spinner_item, 0, options).apply {
                setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
            }
            setSelection(if (arg.configuration.apChannel in 1..165) {
                options.indexOfFirst { it.apChannel == arg.configuration.apChannel }
            } else options.indexOfFirst { it.apBand == arg.configuration.apBand })
        } else dialogView.band_wrapper.isGone = true
    }

    override fun onResume() {
        super.onResume()
        if (!arg.readOnly) validate()
    }

    /**
     * This function is reached only if not arg.readOnly.
     */
    private fun validate() {
        val ssidValid = dialogView.ssid.length() != 0 &&
                Charset.forName("UTF-8").encode(dialogView.ssid.text.toString()).limit() <= 32
        val passwordValid = when (dialogView.security.selectedItemPosition) {
            WifiConfiguration.KeyMgmt.WPA_PSK, WPA2_PSK -> dialogView.password.length() >= 8
            else -> true    // do not try to validate
        }
        dialogView.password_wrapper.error = if (passwordValid) null else {
            requireContext().getString(R.string.credentials_password_too_short)
        }
        (dialog as AlertDialog).getButton(DialogInterface.BUTTON_POSITIVE).isEnabled = ssidValid && passwordValid
    }

    override fun onTextChanged(s: CharSequence, start: Int, before: Int, count: Int) { }
    override fun beforeTextChanged(s: CharSequence, start: Int, count: Int, after: Int) { }
    override fun afterTextChanged(editable: Editable) = validate()
}
