package be.mygod.vpnhotspot

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.databinding.BindingAdapter
import android.support.annotation.DrawableRes
import android.util.Log
import android.widget.ImageView
import java.io.IOException
import java.net.Inet4Address
import java.net.NetworkInterface

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
        (this.interfaceAddresses.asSequence()
                .filter { !it.address.isLinkLocalAddress }
                .map { "${it.address.hostAddress}/${it.networkPrefixLength}" }
                .toList() +
                listOfNotNull(this.hardwareAddress?.joinToString(":") { "%02x".format(it) }))
                .joinToString("\n")

private const val NOISYSU_TAG = "NoisySU"
private const val NOISYSU_SUFFIX = "SUCCESS\n"
fun loggerSu(command: String): String? {
    val process = try {
        ProcessBuilder("su", "-c", command)
                .redirectErrorStream(true)
                .start()
    } catch (e: IOException) {
        return null
    }
    process.waitFor()
    try {
        val err = process.errorStream.bufferedReader().readText()
        if (err.isNotBlank()) Log.e(NOISYSU_TAG, err)
    } catch (e: IOException) {
        e.printStackTrace()
    }
    return try {
        process.inputStream.bufferedReader().readText()
    } catch (e: IOException) {
        e.printStackTrace()
        null
    }
}
fun noisySu(commands: Iterable<String>): Boolean {
    var out = loggerSu("""function noisy() { "$@" || echo "$@" exited with $?; }
${commands.joinToString("\n") { if (it.startsWith("quiet ")) it.substring(6) else "noisy $it" }}
echo $NOISYSU_SUFFIX""")
    val result = out == NOISYSU_SUFFIX
    out = out?.removeSuffix(NOISYSU_SUFFIX)
    if (!out.isNullOrBlank()) Log.i(NOISYSU_TAG, out)
    return result
}
fun noisySu(vararg commands: String) = noisySu(commands.asIterable())
