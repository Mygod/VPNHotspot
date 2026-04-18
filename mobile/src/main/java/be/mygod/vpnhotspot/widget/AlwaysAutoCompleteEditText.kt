package be.mygod.vpnhotspot.widget

import android.content.Context
import android.graphics.Rect
import android.util.AttributeSet
import com.google.android.material.textfield.MaterialAutoCompleteTextView

/**
 * Based on: https://gist.github.com/furycomptuers/4961368
 */
class AlwaysAutoCompleteEditText @JvmOverloads constructor(
    context: Context,
    attrs: AttributeSet? = null,
    defStyleAttr: Int = androidx.appcompat.R.attr.autoCompleteTextViewStyle,
) : MaterialAutoCompleteTextView(context, attrs, defStyleAttr) {
    override fun enoughToFilter() = true

    override fun onFocusChanged(focused: Boolean, direction: Int, previouslyFocusedRect: Rect?) {
        super.onFocusChanged(focused, direction, previouslyFocusedRect)
        if (focused && windowVisibility != GONE && filter != null) {
            performFiltering(text, 0)
            showDropDown()
        }
    }
}
