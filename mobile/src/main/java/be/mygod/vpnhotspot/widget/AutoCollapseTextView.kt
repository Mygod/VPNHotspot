package be.mygod.vpnhotspot.widget

import android.content.Context
import android.support.v7.widget.AppCompatTextView
import android.util.AttributeSet
import android.view.View

class AutoCollapseTextView @JvmOverloads constructor(context: Context, attrs: AttributeSet? = null,
                                                     defStyleAttr: Int = android.R.attr.textViewStyle) :
        AppCompatTextView(context, attrs, defStyleAttr) {
    override fun onTextChanged(text: CharSequence?, start: Int, lengthBefore: Int, lengthAfter: Int) {
        super.onTextChanged(text, start, lengthBefore, lengthAfter)
        visibility = if (text.isNullOrEmpty()) View.GONE else View.VISIBLE
    }
}
