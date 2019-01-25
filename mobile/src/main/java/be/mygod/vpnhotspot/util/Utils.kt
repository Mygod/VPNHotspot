package be.mygod.vpnhotspot.util

import android.content.*
import android.os.Build
import android.system.Os
import android.system.OsConstants
import android.view.View
import android.widget.ImageView
import androidx.annotation.DrawableRes
import androidx.core.view.isVisible
import androidx.databinding.BindingAdapter
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

fun NetworkInterface.formatAddresses() =
        (interfaceAddresses.asSequence()
                .map { "${it.address.hostAddress}/${it.networkPrefixLength}" }
                .toList() +
                listOfNotNull(try {
                    hardwareAddress?.joinToString(":") { "%02x".format(it) }
                } catch (_: SocketException) {
                    null
                }))
                .joinToString("\n")

fun parseNumericAddress(address: String?): InetAddress? =
        Os.inet_pton(OsConstants.AF_INET, address) ?: Os.inet_pton(OsConstants.AF_INET6, address)

fun Context.stopAndUnbind(connection: ServiceConnection) {
    connection.onServiceDisconnected(null)
    unbindService(connection)
}

fun <K, V> HashMap<K, V>.computeIfAbsentCompat(key: K, value: () -> V) = if (Build.VERSION.SDK_INT >= 26)
    computeIfAbsent(key) { value() } else this[key] ?: value().also { put(key, it) }
