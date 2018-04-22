package be.mygod.vpnhotspot.net.wifi

import android.content.Context
import android.content.DialogInterface
import android.net.wifi.WifiConfiguration
import android.net.wifi.WifiConfiguration.AuthAlgorithm
import android.os.Bundle
import android.support.v7.app.AlertDialog
import android.text.Editable
import android.text.InputType
import android.text.TextWatcher
import android.view.View
import android.widget.CheckBox
import android.widget.EditText
import android.widget.TextView
import be.mygod.vpnhotspot.R
import java.nio.charset.Charset

/**
 * https://android.googlesource.com/platform/packages/apps/Settings/+/39b4674/src/com/android/settings/wifi/WifiApDialog.java
 */
class WifiP2pDialog(mContext: Context, private val mListener: DialogInterface.OnClickListener,
                    private val mWifiConfig: WifiConfiguration?) :
        AlertDialog(mContext), View.OnClickListener, TextWatcher {
    companion object {
        private const val BUTTON_SUBMIT = DialogInterface.BUTTON_POSITIVE
    }

    private lateinit var mView: View
    private lateinit var mSsid: TextView
    private lateinit var mPassword: EditText
    /**
     * TODO: SSID in WifiConfiguration for soft ap
     * is being stored as a raw string without quotes.
     * This is not the case on the client side. We need to
     * make things consistent and clean it up
     */
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
        setButton(BUTTON_SUBMIT, context.getString(R.string.wifi_save), mListener)
        setButton(DialogInterface.BUTTON_NEGATIVE,
                context.getString(R.string.wifi_cancel), mListener)
        setButton(DialogInterface.BUTTON_NEUTRAL, context.getString(R.string.repeater_reset_credentials), mListener)
        if (mWifiConfig != null) {
            mSsid.text = mWifiConfig.SSID
            mPassword.setText(mWifiConfig.preSharedKey)
        }
        mSsid.addTextChangedListener(this)
        mPassword.addTextChangedListener(this)
        (mView.findViewById(R.id.show_password) as CheckBox).setOnClickListener(this)
        super.onCreate(savedInstanceState)
        validate()
    }

    override fun onRestoreInstanceState(savedInstanceState: Bundle) {
        super.onRestoreInstanceState(savedInstanceState)
        mPassword.inputType = InputType.TYPE_CLASS_TEXT or
                if ((mView.findViewById(R.id.show_password) as CheckBox).isChecked)
                    InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD
                else InputType.TYPE_TEXT_VARIATION_PASSWORD
    }

    private fun validate() {
        val mSsidString = mSsid.text.toString()
        getButton(BUTTON_SUBMIT).isEnabled = mSsid.length() != 0 &&
                mPassword.length() >= 8 && Charset.forName("UTF-8").encode(mSsidString).limit() <= 32
    }

    override fun onClick(view: View) {
        mPassword.inputType = InputType.TYPE_CLASS_TEXT or if ((view as CheckBox).isChecked)
            InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD
        else
            InputType.TYPE_TEXT_VARIATION_PASSWORD
    }

    override fun onTextChanged(s: CharSequence, start: Int, before: Int, count: Int) { }
    override fun beforeTextChanged(s: CharSequence, start: Int, count: Int, after: Int) { }
    override fun afterTextChanged(editable: Editable) {
        validate()
    }
}
