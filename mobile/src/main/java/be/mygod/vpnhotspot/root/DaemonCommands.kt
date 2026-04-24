package be.mygod.vpnhotspot.root

import android.os.Parcelable
import android.system.Os
import android.system.OsConstants
import be.mygod.librootkotlinx.RootCommandNoResult
import kotlinx.parcelize.Parcelize
import java.io.File
import java.io.IOException

@Parcelize
data class RunDaemon(private val path: String, private val socketPath: String) :
        RootCommandNoResult {
    override suspend fun execute(): Parcelable? {
        val devNull = File("/dev/null")
        val log = File("$socketPath.log")
        log.delete()
        ProcessBuilder(path, "--socket-path", socketPath).fixPath(false)
            .redirectInput(ProcessBuilder.Redirect.from(devNull))
            .redirectOutput(ProcessBuilder.Redirect.appendTo(log))
            .redirectError(ProcessBuilder.Redirect.appendTo(log))
            .start()
        return null
    }
}

@Parcelize
data class KillDaemon(private val path: String) : RootCommandNoResult {
    override suspend fun execute(): Parcelable? {
        for (process in File("/proc").listFiles { _, name -> name.all(Char::isDigit) }!!) {
            val cmdline = try {
                File(process, "cmdline").inputStream().bufferedReader().use { it.readText() }
            } catch (_: IOException) {
                continue
            }
            val executable = cmdline.split(Char.MIN_VALUE, limit = 2).firstOrNull().orEmpty()
            if (executable == path) try {
                Os.kill(process.name.toInt(), OsConstants.SIGTERM)
            } catch (_: Exception) { }
        }
        return null
    }
}
