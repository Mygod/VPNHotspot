package be.mygod.vpnhotspot.widget

import android.text.Spannable
import android.text.method.LinkMovementMethod
import android.text.style.URLSpan
import android.view.MotionEvent
import android.widget.TextView

/**
 * Based on: https://stackoverflow.com/a/32443884/2245107
 */
abstract class TextViewLinkHandler : LinkMovementMethod() {
    companion object {
        fun create(handler: (String) -> Unit) = object : TextViewLinkHandler() {
            override fun onLinkClick(url: String) = handler(url)
        }
    }

    override fun onTouchEvent(widget: TextView, buffer: Spannable, event: MotionEvent): Boolean {
        if (event.action != MotionEvent.ACTION_UP) return super.onTouchEvent(widget, buffer, event)
        val x = event.x - widget.totalPaddingLeft + widget.scrollX
        val y = event.y.toInt() - widget.totalPaddingTop + widget.scrollY
        val layout = widget.layout
        val line = layout.getLineForVertical(y)
        val off = layout.getOffsetForHorizontal(line, x)
        val link = buffer.getSpans(off, off, URLSpan::class.java)
        if (link.isNotEmpty()) onLinkClick(link[0].url)
        return true
    }

    abstract fun onLinkClick(url: String)
}
