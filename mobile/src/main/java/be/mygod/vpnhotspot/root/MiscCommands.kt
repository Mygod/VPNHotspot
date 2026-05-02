package be.mygod.vpnhotspot.root

import android.content.Context
import android.net.TetheringManager
import android.os.Build
import android.os.Parcelable
import android.os.ParcelFileDescriptor
import android.os.RemoteException
import android.provider.Settings
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.ParcelableBoolean
import be.mygod.librootkotlinx.ParcelableInt
import be.mygod.librootkotlinx.ParcelableString
import be.mygod.librootkotlinx.RootCommand
import be.mygod.librootkotlinx.RootCommandNoResult
import be.mygod.librootkotlinx.RootFlow
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.io.forEachLineSafely
import be.mygod.vpnhotspot.io.openReadChannel
import be.mygod.vpnhotspot.net.Routing.Companion.IP
import be.mygod.vpnhotspot.net.Routing.Companion.IPTABLES
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.util.Services
import io.ktor.utils.io.ByteReadChannel
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.onFailure
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.channelFlow
import kotlinx.coroutines.joinAll
import kotlinx.coroutines.launch
import kotlinx.coroutines.runInterruptible
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import java.io.File
import java.io.FileOutputStream
import java.io.IOException

fun ProcessBuilder.fixPath(redirect: Boolean = false) = apply {
    environment().compute("PATH") { _, value ->
        if (value.isNullOrEmpty()) "/system/bin" else "$value:/system/bin"
    }
    redirectErrorStream(redirect)
}

suspend fun <T> ProcessBuilder.withOutputChannels(
    block: suspend (Process, ByteReadChannel, ByteReadChannel) -> T,
): T {
    val stdoutPipe = ParcelFileDescriptor.createPipe()
    val stderrPipe = ParcelFileDescriptor.createPipe()
    var stdoutRead: ParcelFileDescriptor? = stdoutPipe[0]
    var stdoutWrite: ParcelFileDescriptor? = stdoutPipe[1]
    var stderrRead: ParcelFileDescriptor? = stderrPipe[0]
    var stderrWrite: ParcelFileDescriptor? = stderrPipe[1]
    var stdoutChannel: ByteReadChannel? = null
    var stderrChannel: ByteReadChannel? = null
    var process: Process? = null
    var started = false
    try {
        redirectOutput(ProcessBuilder.Redirect.to(File("/proc/self/fd/${stdoutWrite!!.fd}")))
        redirectError(ProcessBuilder.Redirect.to(File("/proc/self/fd/${stderrWrite!!.fd}")))
        process = start()
        val stdout = stdoutRead!!.openReadChannel(Services.mainHandler.looper).also {
            stdoutRead = null
            stdoutChannel = it
        }
        val stderr = stderrRead!!.openReadChannel(Services.mainHandler.looper).also {
            stderrRead = null
            stderrChannel = it
        }
        stdoutWrite.close()
        stdoutWrite = null
        stderrWrite.close()
        stderrWrite = null
        started = true
        return try {
            block(process, stdout, stderr)
        } catch (e: Throwable) {
            if (process.isAlive) process.destroyForcibly()
            throw e
        } finally {
            stdout.cancel(null)
            stderr.cancel(null)
            stdoutChannel = null
            stderrChannel = null
        }
    } finally {
        if (!started) process?.destroyForcibly()
        stdoutChannel?.cancel(null)
        stderrChannel?.cancel(null)
        try {
            stdoutRead?.close()
        } catch (_: IOException) { }
        try {
            stdoutWrite?.close()
        } catch (_: IOException) { }
        try {
            stderrRead?.close()
        } catch (_: IOException) { }
        try {
            stderrWrite?.close()
        } catch (_: IOException) { }
    }
}

@Parcelize
data class Dump(val path: String, val cacheDir: File = app.deviceStorage.codeCacheDir) : RootCommandNoResult {
    override suspend fun execute() = withContext(Dispatchers.IO) {
        FileOutputStream(path, true).use { out ->
            val process = ProcessBuilder("sh").fixPath(true).start()
            process.outputStream.bufferedWriter().use { commands ->
                commands.appendLine("""
                    |echo dumpsys ${Context.WIFI_P2P_SERVICE}
                    |dumpsys ${Context.WIFI_P2P_SERVICE}
                    |echo
                    |echo dumpsys ${Context.CONNECTIVITY_SERVICE} tethering
                    |dumpsys ${Context.CONNECTIVITY_SERVICE} tethering
                    |echo
                    |echo iptables -t filter
                    |iptables-save -t filter
                    |echo
                    |echo iptables -t nat
                    |iptables-save -t nat
                    |echo
                    |echo ip6tables-save
                    |ip6tables-save
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
                    |$IPTABLES -t nat -nvx -L POSTROUTING
                    |echo
                    |echo iptables -t nat -nvx -L vpnhotspot_masquerade
                    |$IPTABLES -t nat -nvx -L vpnhotspot_masquerade
                    |echo
                    |echo iptables -nvx -L vpnhotspot_acl
                    |$IPTABLES -nvx -L vpnhotspot_acl
                    |echo
                    |echo iptables -nvx -L vpnhotspot_stats
                    |$IPTABLES -nvx -L vpnhotspot_stats
                    |echo
                    |echo logcat-su
                    |logcat -d
                """.trimMargin())
            }
            process.inputStream.copyTo(out)
            when (val exit = process.waitFor()) {
                0 -> { }
                else -> out.write("Process exited with $exit".toByteArray())
            }
        }
        null
    }
}

sealed class ProcessData : Parcelable {
    @Parcelize
    data class StdoutLine(val line: String) : ProcessData()
    @Parcelize
    data class StderrLine(val line: String) : ProcessData()
    @Parcelize
    data class Exit(val code: Int) : ProcessData()
}

@Parcelize
class ProcessListener(private val terminateRegex: Regex,
                      private vararg val command: String) : RootFlow<ProcessData> {
    override fun flow() = channelFlow {
        ProcessBuilder(*command).withOutputChannels { process, stdoutChannel, stderrChannel ->
            // we need to destroy process before waiting for the readers during cleanup, so we cannot use coroutineScope
            val stdout = launch(Dispatchers.IO) {
                stdoutChannel.forEachLineSafely { line ->
                    var keepGoing = true
                    trySend(ProcessData.StdoutLine(line)).onFailure {
                        if (it != null) throw it
                        keepGoing = false
                    }
                    if (!keepGoing) return@forEachLineSafely false
                    if (terminateRegex.containsMatchIn(line)) process.destroy()
                    true
                }
            }
            val stderr = launch(Dispatchers.IO) {
                stderrChannel.forEachLineSafely { line ->
                    var keepGoing = true
                    trySend(ProcessData.StderrLine(line)).onFailure {
                        if (it != null) throw it
                        keepGoing = false
                    }
                    keepGoing
                }
            }
            val exit = launch(Dispatchers.IO) {
                trySend(ProcessData.Exit(runInterruptible {
                    process.waitFor()
                })).onFailure {
                    if (it != null) throw it
                    return@launch
                }
            }
            try {
                joinAll(stdout, stderr, exit)
            } finally {
                withContext(NonCancellable) {
                    if (process.isAlive) process.destroyForcibly()
                    stdout.cancel()
                    stderr.cancel()
                    exit.cancel()
                    joinAll(stdout, stderr, exit)
                }
            }
        }
    }.buffer(Channel.UNLIMITED)
}

@Parcelize
class ReadArp : RootCommand<ParcelableString> {
    override suspend fun execute() = withContext(Dispatchers.IO) {
        ParcelableString(File("/proc/net/arp").readText())
    }
}

@Parcelize
@RequiresApi(30)
data class StartTethering(private val type: Int,
                          private val showProvisioningUi: Boolean) : RootCommand<ParcelableInt?> {
    override suspend fun execute(): ParcelableInt? {
        val future = CompletableDeferred<Int?>()
        TetheringManagerCompat.startTethering(type, true, showProvisioningUi, {
            it.run()
        }, object : TetheringManager.StartTetheringCallback {
            override fun onTetheringStarted() {
                future.complete(null)
            }

            override fun onTetheringFailed(error: Int) {
                future.complete(error)
            }
        })
        return future.await()?.let { ParcelableInt(it) }
    }
}

@Parcelize
@RequiresApi(30)
data class StopTethering(private val cacheDir: File, private val type: Int) : RootCommand<ParcelableInt?> {
    override suspend fun execute(): ParcelableInt? {
        val future = CompletableDeferred<Int?>()
        TetheringManagerCompat.stopTethering(type, object : TetheringManagerCompat.StopTetheringCallback {
            override fun onStopTetheringSucceeded() {
                future.complete(null)
            }

            override fun onStopTetheringFailed(error: Int) {
                future.complete(error)
            }

            override fun onException(e: Exception) {
                future.completeExceptionally(e)
            }
        }, Services.context, cacheDir)
        return future.await()?.let { ParcelableInt(it) }
    }
}

@Deprecated("Old API since API 30")
@Parcelize
@Suppress("DEPRECATION")
data class StartTetheringLegacy(private val cacheDir: File, private val type: Int,
                                private val showProvisioningUi: Boolean) : RootCommand<ParcelableBoolean> {
    override suspend fun execute(): ParcelableBoolean {
        val future = CompletableDeferred<Boolean>()
        val callback = object : TetheringManagerCompat.StartTetheringCallback {
            override fun onTetheringStarted() {
                future.complete(true)
            }

            override fun onTetheringFailed(error: Int?) {
                check(error == null)
                future.complete(false)
            }
        }
        TetheringManagerCompat.startTetheringLegacy(type, showProvisioningUi, callback, cacheDir = cacheDir)
        return ParcelableBoolean(future.await())
    }
}

@Parcelize
data class StopTetheringLegacy(private val type: Int) : RootCommandNoResult {
    override suspend fun execute(): Parcelable? {
        TetheringManagerCompat.stopTetheringLegacy(type)
        return null
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

    override suspend fun execute() = withContext(Dispatchers.IO) {
        val process = ProcessBuilder("settings", "put", "global", name, value).fixPath(true).start()
        val error = process.inputStream.bufferedReader().readText()
        val exit = process.waitFor()
        if (exit != 0 || error.isNotEmpty()) throw RemoteException("Process exited with $exit: $error")
        null
    }
}
