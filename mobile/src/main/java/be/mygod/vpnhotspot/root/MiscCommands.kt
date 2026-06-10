package be.mygod.vpnhotspot.root

import android.content.Context
import android.os.RemoteException
import android.provider.Settings
import be.mygod.librootkotlinx.RootCommandNoResult
import be.mygod.librootkotlinx.io.awaitExit
import be.mygod.librootkotlinx.io.openReadChannel
import be.mygod.librootkotlinx.io.openWriteChannel
import be.mygod.librootkotlinx.io.startPipes
import be.mygod.vpnhotspot.util.Services
import io.ktor.utils.io.ByteReadChannel
import io.ktor.utils.io.ByteWriteChannel
import io.ktor.utils.io.toByteArray
import io.ktor.utils.io.writeFully
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import java.io.File

@Parcelize
data class Dump(val path: String) : RootCommandNoResult {
    companion object {
        private const val DUMPSYS = "/system/bin/dumpsys"
        private const val IP = "/system/bin/ip"
        private const val IPTABLES = "/system/bin/iptables"
        const val LOGCAT = "/system/bin/logcat"
    }

    override suspend fun execute() = withContext(Dispatchers.IO) {
        val output = File(path)
        ProcessBuilder("/system/bin/sh").apply {
            redirectErrorStream(true)
            redirectOutput(ProcessBuilder.Redirect.appendTo(output))
        }.startPipes(stdout = false, stderr = false).use { pipes ->
            var stdin: ByteWriteChannel? = null
            try {
                stdin = pipes.requireStdin().openWriteChannel(Services.mainHandler)
                stdin.writeFully("""
                    |echo
                    |echo dumpsys ${Context.WIFI_P2P_SERVICE}
                    |$DUMPSYS ${Context.WIFI_P2P_SERVICE}
                    |echo
                    |echo dumpsys ${Context.CONNECTIVITY_SERVICE} tethering
                    |$DUMPSYS ${Context.CONNECTIVITY_SERVICE} tethering
                    |echo
                    |echo iptables-save
                    |/system/bin/iptables-save
                    |echo
                    |echo ip6tables-save
                    |/system/bin/ip6tables-save
                    |echo
                    |echo ip rule
                    |$IP rule
                    |echo
                    |echo ip route show table all
                    |$IP route show table all
                    |echo
                    |echo ip neigh
                    |$IP neigh
                    |echo
                    |echo ip -s link
                    |$IP -s link
                    |echo
                    |echo iptables -t nat -nvx -L POSTROUTING
                    |$IPTABLES -w -t nat -nvx -L POSTROUTING
                    |echo
                    |echo iptables -t nat -nvx -L vpnhotspot_masquerade
                    |$IPTABLES -w -t nat -nvx -L vpnhotspot_masquerade
                    |echo
                    |echo iptables -nvx -L vpnhotspot_acl
                    |$IPTABLES -w -nvx -L vpnhotspot_acl
                    |echo
                    |echo iptables -nvx -L vpnhotspot_stats
                    |$IPTABLES -w -nvx -L vpnhotspot_stats
                    |echo
                    |echo logcat-su
                    |$LOGCAT -d
                """.trimMargin().encodeToByteArray())
                stdin.flushAndClose()
                stdin = null
                when (val exit = pipes.process.awaitExit()) {
                    0 -> { }
                    else -> output.appendText("Process exited with $exit")
                }
            } finally {
                stdin?.cancel(null)
            }
        }
        null
    }
}

@Parcelize
data class SettingsGlobalPut(val name: String, val value: String) : RootCommandNoResult {
    companion object {
        suspend fun int(name: String, value: Int) {
            try {
                check(Settings.Global.putInt(Services.context.contentResolver, name, value))
            } catch (e: SecurityException) {
                try {
                    RootManager.use { it.execute(SettingsGlobalPut(name, value.toString())) }
                } catch (eRoot: Exception) {
                    eRoot.addSuppressed(e)
                    throw eRoot
                }
            }
        }
    }

    override suspend fun execute() = null.also {
        val (exit, output) = ProcessBuilder("/system/bin/settings", "put", "global", name, value).apply {
            redirectInput(ProcessBuilder.Redirect.from(File("/dev/null")))
        }.startPipes(stdin = false).use { pipes ->
            var stdout: ByteReadChannel? = null
            var stderr: ByteReadChannel? = null
            try {
                stdout = pipes.requireStdout().openReadChannel(Services.mainHandler)
                stderr = pipes.requireStderr().openReadChannel(Services.mainHandler)
                coroutineScope {
                    val stdoutText = async { stdout.toByteArray().decodeToString() }
                    val stderrText = async { stderr.toByteArray().decodeToString() }
                    pipes.process.awaitExit() to stdoutText.await() + stderrText.await()
                }
            } finally {
                stdout?.cancel(null)
                stderr?.cancel(null)
            }
        }
        if (exit != 0 || output.isNotEmpty()) throw RemoteException("Process exited with $exit: $output")
    }
}
