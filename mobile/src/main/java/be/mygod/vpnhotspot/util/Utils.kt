package be.mygod.vpnhotspot.util

import android.content.*
import android.util.Log
import android.view.View
import android.widget.ImageView
import androidx.annotation.DrawableRes
import androidx.core.view.isVisible
import androidx.databinding.BindingAdapter
import be.mygod.vpnhotspot.BuildConfig
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.net.NetworkInterface
import java.net.SocketException

fun debugLog(tag: String?, message: String?) {
    if (BuildConfig.DEBUG) Log.d(tag, message)
    Timber.v(message)
}

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
                } catch (e: SocketException) {
                    e.printStackTrace()
                    Timber.e(e)
                    null
                }))
                .joinToString("\n")

/**
 * Wrapper for kotlin.concurrent.thread that silences uncaught exceptions.
 */
fun thread(name: String? = null, start: Boolean = true, isDaemon: Boolean = false,
           contextClassLoader: ClassLoader? = null, priority: Int = -1, block: () -> Unit): Thread {
    val thread = kotlin.concurrent.thread(false, isDaemon, contextClassLoader, name, priority, block)
    thread.setUncaughtExceptionHandler { _, e ->
        SmartSnackbar.make(R.string.noisy_su_failure).show()
        Timber.e(e)
    }
    if (start) thread.start()
    return thread
}

fun Context.stopAndUnbind(connection: ServiceConnection) {
    connection.onServiceDisconnected(null)
    unbindService(connection)
}
