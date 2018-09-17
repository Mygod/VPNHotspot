package be.mygod.vpnhotspot.net.wifi

import android.content.DialogInterface
import android.content.Intent
import android.net.wifi.WifiConfiguration
import android.net.wifi.WifiConfiguration.AuthAlgorithm
import android.os.Bundle
import android.text.Editable
import android.text.TextWatcher
import android.view.View
import android.widget.EditText
import android.widget.TextView
import androidx.appcompat.app.AlertDialog
import androidx.fragment.app.DialogFragment
import be.mygod.vpnhotspot.R
import com.google.android.material.textfield.TextInputLayout
import java.nio.charset.Charset

/**
 * Based on: https://android.googlesource.com/platform/packages/apps/Settings/+/39b4674/src/com/android/settings/wifi/WifiApDialog.java
 *
 * This dialog has been deprecated in API 28, but we are still using it since it works better for our purposes.
 * Related: https://android.googlesource.com/platform/packages/apps/Settings/+/defb1183ecb00d6231bac7d934d07f58f90261ea
 */
class WifiP2pDialogFragment : DialogFragment(), TextWatcher, DialogInterface.OnClickListener {
    companion object {
        const val TAG = "WifiP2pDialogFragment"
        const val KEY_CONFIGURATION = "configuration"
        const val KEY_CONFIGURER = "configurer"
    }

    private lateinit var mView: View
    private lateinit var mSsid: TextView
    private lateinit var mPassword: EditText
    private lateinit var configurer: P2pSupplicantConfiguration
    private val config: WifiConfiguration?
        get() {
            val config = WifiConfiguration()
            config.SSID = mSsid.text.toString()
            config.allowedAuthAlgorithms.set(AuthAlgorithm.OPEN)
            if (mPassword.length() != 0) {
                val password = mPassword.text.toString()
                config.preSharedKey = password
            }
            return config
        }

    override fun onCreateDialog(savedInstanceState: Bundle?): AlertDialog =
            AlertDialog.Builder(requireContext()).apply {
                mView = requireActivity().layoutInflater.inflate(R.layout.dialog_wifi_ap, null)
                setView(mView)
                setTitle(R.string.repeater_configure)
                mSsid = mView.findViewById(R.id.ssid)
                mPassword = mView.findViewById(R.id.password)
                setPositiveButton(context.getString(R.string.wifi_save), this@WifiP2pDialogFragment)
                setNegativeButton(context.getString(R.string.wifi_cancel), null)
                setNeutralButton(context.getString(R.string.repeater_reset_credentials), this@WifiP2pDialogFragment)
                val arguments = arguments!!
                configurer = arguments.getParcelable(KEY_CONFIGURER)!!
                val mWifiConfig = arguments.getParcelable<WifiConfiguration>(KEY_CONFIGURATION)
                if (mWifiConfig != null) {
                    mSsid.text = mWifiConfig.SSID
                    mPassword.setText(mWifiConfig.preSharedKey)
                }
                mSsid.addTextChangedListener(this@WifiP2pDialogFragment)
                mPassword.addTextChangedListener(this@WifiP2pDialogFragment)
            }.create()

    override fun onStart() {
        super.onStart()
        validate()
    }

    private fun validate() {
        val mSsidString = mSsid.text.toString()
        val ssidValid = mSsid.length() != 0 && Charset.forName("UTF-8").encode(mSsidString).limit() <= 32
        val passwordValid = mPassword.length() >= 8
        mView.findViewById<TextInputLayout>(R.id.password_wrapper).error =
                if (passwordValid) null else requireContext().getString(R.string.credentials_password_too_short)
        (dialog as AlertDialog).getButton(DialogInterface.BUTTON_POSITIVE).isEnabled = ssidValid && passwordValid
    }

    override fun onTextChanged(s: CharSequence, start: Int, before: Int, count: Int) { }
    override fun beforeTextChanged(s: CharSequence, start: Int, count: Int, after: Int) { }
    override fun afterTextChanged(editable: Editable) = validate()

    override fun onClick(dialog: DialogInterface?, which: Int) {
        targetFragment!!.onActivityResult(targetRequestCode, which, Intent().apply {
            putExtra(KEY_CONFIGURATION, config)
            putExtra(KEY_CONFIGURER, configurer)
        })
    }
}
