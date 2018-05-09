package be.mygod.vpnhotspot.util

import android.util.Log
import be.mygod.vpnhotspot.App
import be.mygod.vpnhotspot.R
import java.io.IOException
import java.io.InputStream

private const val NOISYSU_TAG = "NoisySU"
private const val NOISYSU_SUFFIX = "SUCCESS\n"

fun loggerSuStream(command: String): InputStream? {
    val process = try {
        ProcessBuilder("su", "-c", command)
                .redirectErrorStream(true)
                .directory(App.app.deviceContext.cacheDir)
                .start()
    } catch (e: IOException) {
        e.printStackTrace()
        return null
    }
    thread("LoggerSU-error") {
        val err = process.errorStream.bufferedReader().readText()
        if (err.isNotBlank()) {
            Log.e(NOISYSU_TAG, err)
            App.app.toast(R.string.noisy_su_failure)
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
