package be.mygod.vpnhotspot.preference

import android.content.Context
import android.view.View
import android.view.ViewGroup
import android.widget.ArrayAdapter
import androidx.core.os.bundleOf
import androidx.preference.EditTextPreferenceDialogFragmentCompat
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.widget.AlwaysAutoCompleteEditText

class AlwaysAutoCompleteEditTextPreferenceDialogFragment : EditTextPreferenceDialogFragmentCompat() {
    companion object {
        private const val ARG_SUGGESTIONS = "suggestions"
    }

    fun setArguments(key: String, suggestions: Array<String>) {
        arguments = bundleOf(ARG_KEY to key, ARG_SUGGESTIONS to suggestions)
    }

    private lateinit var editText: AlwaysAutoCompleteEditText

    override fun onCreateDialogView(context: Context) = super.onCreateDialogView(context).apply {
        editText = AlwaysAutoCompleteEditText(context).apply {
            id = android.R.id.edit
            minHeight = resources.getDimensionPixelSize(R.dimen.touch_target_min)
        }
        val oldEditText = findViewById<View>(android.R.id.edit)!!
        val container = oldEditText.parent as ViewGroup
        container.removeView(oldEditText)
        container.addView(editText, oldEditText.layoutParams)
    }

    override fun onBindDialogView(view: View) {
        super.onBindDialogView(view)
        editText.hint = (preference.summaryProvider as SummaryFallbackProvider).fallback
        arguments?.getStringArray(ARG_SUGGESTIONS)?.let { suggestions ->
            editText.setAdapter(ArrayAdapter(view.context, android.R.layout.select_dialog_item, suggestions))
        }
        editText.clearFocus()   // having focus is buggy currently
    }
}
