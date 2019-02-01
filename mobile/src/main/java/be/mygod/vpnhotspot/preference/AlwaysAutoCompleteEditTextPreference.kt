package be.mygod.vpnhotspot.preference

import android.content.Context
import android.text.TextUtils
import android.util.AttributeSet
import androidx.preference.EditTextPreference
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.widget.AlwaysAutoCompleteEditText

open class AlwaysAutoCompleteEditTextPreference @JvmOverloads constructor(
        context: Context, attrs: AttributeSet? = null, defStyleAttr: Int = R.attr.editTextPreferenceStyle,
        defStyleRes: Int = 0) : EditTextPreference(context, attrs, defStyleAttr, defStyleRes) {
    val editText = AlwaysAutoCompleteEditText(context, attrs)

    init {
        editText.id = android.R.id.edit
        SummaryFallbackProvider(this)
    }

    override fun setText(text: String?) {
        val oldText = getText()
        super.setText(text)
        if (!TextUtils.equals(text, oldText)) notifyChanged()
    }
}
