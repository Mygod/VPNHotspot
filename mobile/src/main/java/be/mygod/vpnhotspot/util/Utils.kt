package be.mygod.vpnhotspot.util

import android.content.*
import android.os.Build
import android.system.Os
import android.system.OsConstants
import android.text.Spannable
import android.text.SpannableStringBuilder
import android.text.style.URLSpan
import android.view.View
import android.widget.ImageView
import androidx.annotation.DrawableRes
import androidx.core.net.toUri
import androidx.core.view.isVisible
import androidx.databinding.BindingAdapter
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.room.macToString
import be.mygod.vpnhotspot.widget.SmartSnackbar
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketException

/**
 * This is a hack: we wrap longs around in 1 billion and such. Hopefully every language counts in base 10 and this works
 * marvelously for everybody.
 */
fun Long.toPluralInt(): Int {
    check(this >= 0)    // please don't mess with me
    if (this <= Int.MAX_VALUE) return toInt()
    return (this % 1000000000).toInt() + 1000000000
}

fun CharSequence?.onEmpty(otherwise: CharSequence): CharSequence = if (isNullOrEmpty()) otherwise else this!!

fun broadcastReceiver(receiver: (Context, Intent) -> Unit) = object : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) = receiver(context, intent)
}

fun intentFilter(vararg actions: String): IntentFilter {
    val result = IntentFilter()
    actions.forEach { result.addAction(it) }
    return result
}

@BindingAdapter("android:src")
fun setImageResource(imageView: ImageView, @DrawableRes resource: Int) = imageView.setImageResource(resource)

@BindingAdapter("android:visibility")
fun setVisibility(view: View, value: Boolean) {
    view.isVisible = value
}

fun makeMacSpan(mac: String) = SpannableStringBuilder(mac).apply {
    if (app.hasTouch) setSpan(object : URLSpan("https://macvendors.co/results/$mac") {
        override fun onClick(widget: View) = widget.context.launchUrl(url)
    }, 0, length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
}

fun NetworkInterface.formatAddresses() = SpannableStringBuilder().apply {
    for (address in interfaceAddresses) appendln("${address.address.hostAddress}/${address.networkPrefixLength}")
    try {
        hardwareAddress?.apply { appendln(makeMacSpan(asIterable().macToString())) }
    } catch (_: SocketException) { }
}.trimEnd()

fun parseNumericAddress(address: String?): InetAddress? =
        Os.inet_pton(OsConstants.AF_INET, address) ?: Os.inet_pton(OsConstants.AF_INET6, address)

fun Context.launchUrl(url: String) {
    if (app.hasTouch) try {
        app.customTabsIntent.launchUrl(this, url.toUri())
        return
    } catch (_: ActivityNotFoundException) { } catch (_: SecurityException) { }
    SmartSnackbar.make(url).show()
}

fun Context.stopAndUnbind(connection: ServiceConnection) {
    connection.onServiceDisconnected(null)
    unbindService(connection)
}

fun <K, V> HashMap<K, V>.computeIfAbsentCompat(key: K, value: () -> V) = if (Build.VERSION.SDK_INT >= 26)
    computeIfAbsent(key) { value() } else this[key] ?: value().also { put(key, it) }
