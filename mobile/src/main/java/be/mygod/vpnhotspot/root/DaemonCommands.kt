package be.mygod.vpnhotspot.root

import android.os.Parcelable
import be.mygod.librootkotlinx.RootCommandNoResult
import kotlinx.parcelize.Parcelize
import java.io.File

@Parcelize
data class RunDaemon(
    private val path: String,
    private val socketName: String,
    private val connectionFile: String,
    private val logPath: String,
) : RootCommandNoResult {
    override suspend fun execute(): Parcelable? {
        val devNull = File("/dev/null")
        val log = File(logPath)
        log.delete()
        ProcessBuilder(path, "--socket-name", socketName, "--connection-file", connectionFile).fixPath(false)
            .redirectInput(ProcessBuilder.Redirect.from(devNull))
            .redirectOutput(ProcessBuilder.Redirect.appendTo(log))
            .redirectError(ProcessBuilder.Redirect.appendTo(log))
            .start()
        return null
    }
}
