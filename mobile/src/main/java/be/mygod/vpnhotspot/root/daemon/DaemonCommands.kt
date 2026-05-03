package be.mygod.vpnhotspot.root.daemon

import android.os.ParcelFileDescriptor
import android.os.Parcelable
import be.mygod.librootkotlinx.RootCommand
import kotlinx.parcelize.Parcelize
import java.io.File

@Parcelize
data class RunDaemon(
    private val command: List<String>,
    private val socketName: String,
    private val stdout: ParcelFileDescriptor,
    private val stderr: ParcelFileDescriptor,
) : RootCommand<Parcelable?> {
    override suspend fun execute() = null.also {
        stdout.use { stdout ->
            stderr.use { stderr ->
                ProcessBuilder(command + socketName)
                    .apply { environment()["RUST_BACKTRACE"] = "1" }
                    .redirectInput(ProcessBuilder.Redirect.from(File("/dev/null")))
                    // Opened before fork, then dup2'd onto the child stdio fds.
                    .redirectOutput(ProcessBuilder.Redirect.to(File("/proc/self/fd/${stdout.fd}")))
                    .redirectError(ProcessBuilder.Redirect.to(File("/proc/self/fd/${stderr.fd}")))
                    .start()
            }
        }
    }
}
