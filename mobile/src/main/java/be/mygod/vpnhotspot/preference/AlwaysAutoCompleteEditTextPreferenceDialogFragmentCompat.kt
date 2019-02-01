package be.mygod.vpnhotspot.preference

import android.view.View
import android.view.ViewGroup
import android.widget.ArrayAdapter
import android.widget.EditText
import androidx.appcompat.widget.AppCompatAutoCompleteTextView
import androidx.core.os.bundleOf
import androidx.preference.PreferenceDialogFragmentCompat

open class AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat : PreferenceDialogFragmentCompat() {
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
        if (oldParent !== view) {
            oldParent?.removeView(editText)
            onAddEditTextToDialogView(view, editText)
        }
    }

    override fun needInputMethod(): Boolean = true

    protected open fun onAddEditTextToDialogView(dialogView: View, editText: EditText) {
        val oldEditText = dialogView.findViewById<View>(android.R.id.edit)
        if (oldEditText != null) {
            val container = oldEditText.parent as? ViewGroup?
            if (container != null) {
                container.removeView(oldEditText)
                container.addView(editText, ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT)
            }
        }
    }

    override fun onDialogClosed(positiveResult: Boolean) {
        if (positiveResult) {
            val value = this.editText.text.toString()
            if (this.editTextPreference.callChangeListener(value)) {
                this.editTextPreference.text = value
            }
        }
    }
}
