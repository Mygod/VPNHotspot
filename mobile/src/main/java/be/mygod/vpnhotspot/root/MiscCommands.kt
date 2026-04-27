package be.mygod.vpnhotspot.root

import android.content.Context
import android.net.TetheringManager
import android.os.Build
import android.os.Parcelable
import android.os.RemoteException
import android.provider.Settings
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.ParcelableBoolean
import be.mygod.librootkotlinx.ParcelableInt
import be.mygod.librootkotlinx.ParcelableString
import be.mygod.librootkotlinx.RootCommand
import be.mygod.librootkotlinx.RootFlow
import be.mygod.librootkotlinx.RootCommandNoResult
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing.Companion.IP
import be.mygod.vpnhotspot.net.Routing.Companion.IPTABLES
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.VpnFirewallManager
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.isEBADF
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
import timber.log.Timber
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.io.InterruptedIOException

fun ProcessBuilder.fixPath(redirect: Boolean = false) = apply {
    environment().compute("PATH") { _, value ->
        if (value.isNullOrEmpty()) "/system/bin" else "$value:/system/bin"
    }
    redirectErrorStream(redirect)
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
                """.trimMargin())
                val dumpCommand = if (Build.VERSION.SDK_INT >= 33) {
                    "dumpsys ${Context.CONNECTIVITY_SERVICE} trafficcontroller"
                } else VpnFirewallManager.DUMP_COMMAND
                commands.appendLine("echo $dumpCommand\n$dumpCommand\necho")
                if (Build.VERSION.SDK_INT >= 31) commands.appendLine(
                    "settings get global ${VpnFirewallManager.UIDS_ALLOWED_ON_RESTRICTED_NETWORKS}")
                commands.appendLine("""
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
                    |echo iptables -nvx -L vpnhotspot_fwd
                    |$IPTABLES -nvx -L vpnhotspot_fwd
                    |echo
                    |echo iptables -nvx -L vpnhotspot_acl
                    |$IPTABLES -nvx -L vpnhotspot_acl
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
        val process = ProcessBuilder(*command).start()
        // we need to destroy process before waiting for the readers during cleanup, so we cannot use coroutineScope
        val stdout = launch(Dispatchers.IO) {
            try {
                process.inputStream.bufferedReader().useLines { lines ->
                    for (line in lines) {
                        trySend(ProcessData.StdoutLine(line)).onFailure {
                            if (it != null) throw it
                            return@useLines
                        }
                        if (terminateRegex.containsMatchIn(line)) process.destroy()
                    }
                }
            } catch (_: InterruptedIOException) { } catch (e: IOException) {
                if (!e.isEBADF) Timber.w(e)
            }
        }
        val stderr = launch(Dispatchers.IO) {
            try {
                process.errorStream.bufferedReader().useLines { lines ->
                    for (line in lines) trySend(ProcessData.StderrLine(line)).onFailure {
                        if (it != null) throw it
                        return@useLines
                    }
                }
            } catch (_: InterruptedIOException) { } catch (e: IOException) {
                if (!e.isEBADF) Timber.w(e)
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
