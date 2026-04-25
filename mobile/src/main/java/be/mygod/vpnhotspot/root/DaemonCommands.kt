package be.mygod.vpnhotspot.root

import android.os.ParcelFileDescriptor
import android.os.Parcelable
import be.mygod.librootkotlinx.RootCommand
import kotlinx.parcelize.Parcelize
import java.io.File

@Parcelize
data class RunDaemon(
    private val command: List<String>,
    private val socketName: String,
    private val connectionFile: String,
    private val stdout: ParcelFileDescriptor,
    private val stderr: ParcelFileDescriptor,
) : RootCommand<Parcelable?> {
    override suspend fun execute() = null.also {
        val fullCommand = (command + listOf(
            "--socket-name", socketName,
            "--connection-file", connectionFile,
        )).toTypedArray()
        stdout.use { stdout ->
            stderr.use { stderr ->
                ParcelFileDescriptor.open(File("/dev/null"), ParcelFileDescriptor.MODE_READ_ONLY).use { devNull ->
                    Jni.launchProcess(fullCommand, devNull.fd, stdout.fd, stderr.fd)
                }
            }
        }
    }
}
