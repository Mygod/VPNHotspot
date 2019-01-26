package be.mygod.vpnhotspot.widget

import android.content.Context
import android.text.method.LinkMovementMethod
import android.util.AttributeSet
import androidx.appcompat.widget.AppCompatTextView

open class LinkTextView @JvmOverloads constructor(context: Context, attrs: AttributeSet? = null,
                                                  defStyleAttr: Int = android.R.attr.textViewStyle) :
        AppCompatTextView(context, attrs, defStyleAttr) {
    override fun setTextIsSelectable(selectable: Boolean) {
        super.setTextIsSelectable(selectable)
        movementMethod = LinkMovementMethod.getInstance()   // override what was set in setTextIsSelectable
    }
}
