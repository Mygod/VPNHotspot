package be.mygod.vpnhotspot.util

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.databinding.BindingAdapter
import android.support.annotation.DrawableRes
import android.util.Log
import android.widget.ImageView
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.BuildConfig
import be.mygod.vpnhotspot.R
import java.io.IOException
import java.io.InputStream
import java.net.NetworkInterface
import java.net.SocketException

fun debugLog(tag: String?, message: String?) {
    if (BuildConfig.DEBUG) Log.d(tag, message)
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

fun NetworkInterface.formatAddresses() =
        (interfaceAddresses.asSequence()
                .map { "${it.address.hostAddress}/${it.networkPrefixLength}" }
                .toList() +
                listOfNotNull(try {
                    hardwareAddress?.joinToString(":") { "%02x".format(it) }
                } catch (e: SocketException) {
                    e.printStackTrace()
                    null
                }))
                .joinToString("\n")

/**
 * Wrapper for kotlin.concurrent.thread that silences uncaught exceptions.
 */
fun thread(name: String? = null, start: Boolean = true, isDaemon: Boolean = false,
           contextClassLoader: ClassLoader? = null, priority: Int = -1, block: () -> Unit): Thread {
    val thread = kotlin.concurrent.thread(false, isDaemon, contextClassLoader, name, priority, block)
    thread.setUncaughtExceptionHandler { _, _ -> app.toast(R.string.noisy_su_failure) }
    if (start) thread.start()
    return thread
}

private const val NOISYSU_TAG = "NoisySU"
private const val NOISYSU_SUFFIX = "SUCCESS\n"
fun loggerSuStream(command: String): InputStream? {
    val process = try {
        ProcessBuilder("su", "-c", command)
                .redirectErrorStream(true)
                .directory(app.deviceContext.cacheDir)
                .start()
    } catch (e: IOException) {
        e.printStackTrace()
        return null
    }
    thread("LoggerSU-error") {
        val err = process.errorStream.bufferedReader().readText()
        if (err.isNotBlank()) {
            Log.e(NOISYSU_TAG, err)
            app.toast(R.string.noisy_su_failure)
        }
    }
    return process.inputStream
}
fun loggerSu(command: String): String? {
    val stream = loggerSuStream(command) ?: return null
    return try {
        stream.bufferedReader().readText()
    } catch (e: IOException) {
        e.printStackTrace()
        null
    }
}
fun noisySu(commands: Iterable<String>): Boolean? {
    var out = loggerSu("""function noisy() { "$@" || echo "$@" exited with $?; }
${commands.joinToString("\n") { if (it.startsWith("quiet ")) it.substring(6) else "noisy $it" }}
echo $NOISYSU_SUFFIX""")
    val result = if (out == null) null else out == NOISYSU_SUFFIX
    out = out?.removeSuffix(NOISYSU_SUFFIX)
    if (!out.isNullOrBlank()) Log.i(NOISYSU_TAG, out)
    return result
}
fun noisySu(vararg commands: String) = noisySu(commands.asIterable())
