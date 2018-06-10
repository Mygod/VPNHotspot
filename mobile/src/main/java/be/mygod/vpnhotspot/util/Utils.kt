package be.mygod.vpnhotspot.util

import android.content.*
import android.databinding.BindingAdapter
import android.os.Bundle
import android.support.annotation.DrawableRes
import android.support.v4.content.ContextCompat
import android.util.Log
import android.view.View
import android.widget.ImageView
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.BuildConfig
import be.mygod.vpnhotspot.R
import com.crashlytics.android.Crashlytics
import java.net.NetworkInterface
import java.net.SocketException

fun debugLog(tag: String?, message: String?) {
    if (BuildConfig.DEBUG) Log.d(tag, message)
    Crashlytics.log("$tag: $message")
}

fun broadcastReceiver(receiver: (Context, Intent) -> Unit) = object : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) = receiver(context, intent)
}

fun intentFilter(vararg actions: String): IntentFilter {
    val result = IntentFilter()
    actions.forEach { result.addAction(it) }
    return result
}

inline fun <reified T> Context.systemService() = ContextCompat.getSystemService(this, T::class.java)!!

fun Bundle.put(key: String, map: Array<String>): Bundle {
    putStringArray(key, map)
    return this
}

@BindingAdapter("android:src")
fun setImageResource(imageView: ImageView, @DrawableRes resource: Int) = imageView.setImageResource(resource)

@BindingAdapter("android:visibility")
fun setVisibility(view: View, value: Boolean) {
    view.visibility = if (value) View.VISIBLE else View.GONE
}

fun NetworkInterface.formatAddresses() =
        (interfaceAddresses.asSequence()
                .map { "${it.address.hostAddress}/${it.networkPrefixLength}" }
                .toList() +
                listOfNotNull(try {
                    hardwareAddress?.joinToString(":") { "%02x".format(it) }
                } catch (e: SocketException) {
                    e.printStackTrace()
                    Crashlytics.logException(e)
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
        app.toast(R.string.noisy_su_failure)
        Crashlytics.logException(e)
    }
    if (start) thread.start()
    return thread
}

fun Context.stopAndUnbind(connection: ServiceConnection) {
    connection.onServiceDisconnected(null)
    unbindService(connection)
}
