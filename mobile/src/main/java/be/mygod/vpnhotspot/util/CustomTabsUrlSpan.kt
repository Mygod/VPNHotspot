package be.mygod.vpnhotspot.util

import android.text.style.URLSpan
import android.view.View

class CustomTabsUrlSpan(url: String) : URLSpan(url) {
    override fun onClick(widget: View) = widget.context.launchUrl(url)
}
