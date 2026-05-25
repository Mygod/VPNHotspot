package be.mygod.vpnhotspot.root.daemon

import android.os.Process
import android.system.Os
import java.io.IOException

object DaemonAbi {
    fun check(
        daemonPath: String,
        machine: String = Os.uname().machine,
        is64Bit: Boolean = Process.is64Bit(),
    ) {
        val rootAbi = rootAbi(machine, is64Bit) ?: return
        val daemonAbi = daemonAbi(daemonPath) ?: return
        if (daemonAbi != rootAbi) throw IOException("Wrong APK variant installed. Install the $rootAbi APK instead of ${
            daemonAbi}.")
    }

    fun daemonAbi(path: String): String? =
        path.splitToSequence('/').dropWhile { it != "lib" }.drop(1).firstOrNull()?.let(DaemonAbi::normalizeAbi)

    fun rootAbi(machine: String, is64Bit: Boolean) = when (machine) {
        "aarch64" -> if (is64Bit) "arm64-v8a" else "armeabi-v7a"
        "x86_64" -> if (is64Bit) "x86_64" else "x86"
        "i386", "i486", "i586", "i686" -> "x86"
        else -> if (machine.startsWith("armv")) "armeabi-v7a" else null
    }

    private fun normalizeAbi(value: String) = when (value) {
        "arm64" -> "arm64-v8a"
        "arm" -> "armeabi-v7a"
        "i386", "i486", "i586", "i686" -> "x86"
        "arm64-v8a", "armeabi-v7a", "x86", "x86_64" -> value
        else -> null
    }
}
