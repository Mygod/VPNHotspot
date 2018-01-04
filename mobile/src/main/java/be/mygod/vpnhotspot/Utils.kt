package be.mygod.vpnhotspot

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.Bundle
import android.util.Log
import java.io.InputStream

fun broadcastReceiver(receiver: (Context, Intent) -> Unit) = object : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) = receiver(context, intent)
}

fun intentFilter(vararg actions: String): IntentFilter {
    val result = IntentFilter()
    actions.forEach { result.addAction(it) }
    return result
}

fun Bundle.put(key: String, map: Array<String>): Bundle {
    putStringArray(key, map)
    return this
}

const val NOISYSU_TAG = "NoisySU"
const val NOISYSU_SUFFIX = "SUCCESS\n"
fun loggerSuStream(command: String): InputStream {
    val process = ProcessBuilder("su", "-c", command)
            .redirectErrorStream(true)
            .start()
    process.waitFor()
    val err = process.errorStream.bufferedReader().use { it.readText() }
    if (!err.isBlank()) Log.e(NOISYSU_TAG, err)
    return process.inputStream
}
fun loggerSu(command: String): String = loggerSuStream(command).bufferedReader().use { it.readText() }
fun noisySu(commands: Iterable<String>): Boolean {
    var out = loggerSu("""function noisy() { "$@" || echo "$@" exited with $?; }
${commands.joinToString("\n") { if (it.startsWith("while ")) it else "noisy $it" }}
echo $NOISYSU_SUFFIX""")
    val result = out == NOISYSU_SUFFIX
    out = out.removeSuffix(NOISYSU_SUFFIX)
    if (!out.isBlank()) Log.i(NOISYSU_TAG, out)
    return result
}
fun noisySu(vararg commands: String) = noisySu(commands.asIterable())
