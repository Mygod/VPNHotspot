package be.mygod.vpnhotspot.net.wifi

import android.content.Context
import android.content.DialogInterface
import android.net.wifi.WifiConfiguration
import android.net.wifi.WifiConfiguration.AuthAlgorithm
import android.os.Build
import android.os.Bundle
import com.google.android.material.textfield.TextInputLayout
import androidx.appcompat.app.AlertDialog
import android.text.Editable
import android.text.TextWatcher
import android.view.View
import android.widget.EditText
import android.widget.TextView
import be.mygod.vpnhotspot.R
import java.nio.charset.Charset

/**
 * https://android.googlesource.com/platform/packages/apps/Settings/+/39b4674/src/com/android/settings/wifi/WifiApDialog.java
 */
class WifiP2pDialog(mContext: Context, private val mListener: DialogInterface.OnClickListener,
                    private val mWifiConfig: WifiConfiguration?) : AlertDialog(mContext), TextWatcher {
    companion object {
        private const val BUTTON_SUBMIT = DialogInterface.BUTTON_POSITIVE
    }

    private lateinit var mView: View
    private lateinit var mSsid: TextView
    private lateinit var mPassword: EditText
    val config: WifiConfiguration?
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

    override fun onCreate(savedInstanceState: Bundle?) {
        mView = layoutInflater.inflate(R.layout.dialog_wifi_ap, null)
        setView(mView)
        val context = context
        setTitle(R.string.repeater_configure)
        mSsid = mView.findViewById(R.id.ssid)
        mPassword = mView.findViewById(R.id.password)
        // Note: Reading persistent group information in p2p_supplicant.conf wasn't available until this commit:
        // https://android.googlesource.com/platform/external/wpa_supplicant_8/+/216983bceec7c450951e2fbcd076b5c75d432e57%5E%21/
        // which isn't merged until Android 6.0.
        if (Build.VERSION.SDK_INT >= 23) setButton(BUTTON_SUBMIT, context.getString(R.string.wifi_save), mListener)
        setButton(DialogInterface.BUTTON_NEGATIVE,
                context.getString(R.string.wifi_cancel), mListener)
        setButton(DialogInterface.BUTTON_NEUTRAL, context.getString(R.string.repeater_reset_credentials), mListener)
        if (mWifiConfig != null) {
            mSsid.text = mWifiConfig.SSID
            mPassword.setText(mWifiConfig.preSharedKey)
        }
        mSsid.addTextChangedListener(this)
        mPassword.addTextChangedListener(this)
        super.onCreate(savedInstanceState)
        validate()
    }

    private fun validate() {
        val mSsidString = mSsid.text.toString()
        val ssidValid = mSsid.length() != 0 && Charset.forName("UTF-8").encode(mSsidString).limit() <= 32
        val passwordValid = mPassword.length() >= 8
        mView.findViewById<TextInputLayout>(R.id.password_wrapper).error =
                if (passwordValid) null else context.getString(R.string.credentials_password_too_short)
        getButton(BUTTON_SUBMIT).isEnabled = ssidValid && passwordValid
    }

    override fun onTextChanged(s: CharSequence, start: Int, before: Int, count: Int) { }
    override fun beforeTextChanged(s: CharSequence, start: Int, count: Int, after: Int) { }
    override fun afterTextChanged(editable: Editable) {
        validate()
    }
}
