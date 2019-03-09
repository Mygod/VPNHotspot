package be.mygod.vpnhotspot.preference

import android.view.View
import android.view.ViewGroup
import android.widget.ArrayAdapter
import androidx.appcompat.widget.AppCompatAutoCompleteTextView
import androidx.core.os.bundleOf
import androidx.preference.PreferenceDialogFragmentCompat

class AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat : PreferenceDialogFragmentCompat() {
    companion object {
        private const val ARG_SUGGESTIONS = "suggestions"
    }

    fun setArguments(key: String, suggestions: Array<String>) {
        arguments = bundleOf(Pair(ARG_KEY, key), Pair(ARG_SUGGESTIONS, suggestions))
    }

    private lateinit var editText: AppCompatAutoCompleteTextView
    private val editTextPreference get() = this.preference as AlwaysAutoCompleteEditTextPreference

    override fun onBindDialogView(view: View) {
        super.onBindDialogView(view)

        editText = editTextPreference.editText
        editText.hint = (editTextPreference.summaryProvider as SummaryFallbackProvider).fallback
        editText.setText(this.editTextPreference.text)

        val text = editText.text
        if (text != null) editText.setSelection(text.length, text.length)

        val suggestions = arguments?.getStringArray(ARG_SUGGESTIONS)
        if (suggestions != null)
            editText.setAdapter(ArrayAdapter(view.context, android.R.layout.select_dialog_item, suggestions))

        val oldParent = editText.parent as? ViewGroup?
        if (oldParent === view) return
        oldParent?.removeView(editText)
        val oldEditText = view.findViewById<View>(android.R.id.edit) ?: return
        val container = oldEditText.parent as? ViewGroup? ?: return
        container.removeView(oldEditText)
        container.addView(editText, oldEditText.layoutParams)
    }

    override fun needInputMethod(): Boolean = true

    override fun onDialogClosed(positiveResult: Boolean) {
        if (positiveResult) {
            val value = this.editText.text.toString()
            if (this.editTextPreference.callChangeListener(value)) {
                this.editTextPreference.text = value
            }
        }
    }
}
