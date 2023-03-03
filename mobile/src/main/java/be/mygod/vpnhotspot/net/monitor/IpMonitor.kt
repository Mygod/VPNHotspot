package be.mygod.vpnhotspot.net.monitor

import android.os.Build
import androidx.core.content.edit
import be.mygod.librootkotlinx.RootServer
import be.mygod.librootkotlinx.isEBADF
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.root.ProcessData
import be.mygod.vpnhotspot.root.ProcessListener
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.RoutingCommands
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.ReceiveChannel
import kotlinx.coroutines.channels.consumeEach
import timber.log.Timber
import java.io.IOException
import java.io.InterruptedIOException
import kotlin.concurrent.thread

abstract class IpMonitor {
    companion object {
        const val KEY = "service.ipMonitor"
        // https://android.googlesource.com/platform/external/iproute2/+/7f7a711/lib/libnetlink.c#493
        private val errorMatcher = ("(?:^Cannot (?:bind netlink socket|send dump request): |^request send failed: |" +
                "Dump (was interrupted and may be inconsistent.|terminated)$)").toRegex()
        var currentMode: Mode
            get() {
                // Completely restricted on Android 13: https://github.com/termux/termux-app/issues/2993#issuecomment-1250312777
                val defaultMode = if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
                    Mode.Poll
                } else Mode.MonitorRoot
                return Mode.valueOf(app.pref.getString(KEY, defaultMode.toString()) ?: "")
            }
            set(value) = app.pref.edit { putString(KEY, value.toString()) }
    }

    enum class Mode(val isMonitor: Boolean = false) {
        @Deprecated("No longer usable on API 30+")
        Monitor(true),
        MonitorRoot(true),
        @Deprecated("No longer usable on API 30+")
        Poll,
        PollRoot,
    }

    private class FlushFailure : RuntimeException()
    protected abstract val monitoredObject: String
    protected abstract suspend fun processLine(line: String)
    protected abstract suspend fun processLines(lines: Sequence<String>)

    @Volatile
    private var destroyed = false
    private var monitor: Process? = null
    private lateinit var worker: Job

    @Suppress("BlockingMethodInNonBlockingContext")
    private suspend fun handleProcess(builder: ProcessBuilder) {
        val process = try {
            builder.start()
        } catch (e: IOException) {
            Timber.d(e)
            return
        }
        monitor = process
        coroutineScope {
            launch(Dispatchers.IO) {
                try {
                    process.errorStream.bufferedReader().forEachLine { Timber.e(it) }
                } catch (_: InterruptedIOException) { } catch (e: IOException) {
                    if (!e.isEBADF) Timber.w(e)
                }
            }
            try {
                process.inputStream.bufferedReader().useLines { lines ->
                    for (line in lines) if (errorMatcher.containsMatchIn(line)) {
                        Timber.w(line)
                        process.destroy()
                        break   // move on to next mode
                    } else processLine(line)
                }
            } catch (_: InterruptedIOException) { } catch (e: IOException) {
                if (!e.isEBADF) Timber.w(e)
            }
        }
        Timber.d("Monitor process exited with ${process.waitFor()}")
    }
    private suspend fun handleChannel(channel: ReceiveChannel<ProcessData>) {
        channel.consumeEach {
            when (it) {
                is ProcessData.StdoutLine -> if (errorMatcher.containsMatchIn(it.line)) {
                    Timber.w(it.line)
                } else processLine(it.line)
                is ProcessData.StderrLine -> Timber.e(it.line)
                is ProcessData.Exit -> Timber.d("Root monitor process exited with ${it.code}")
            }
        }
    }

    protected fun init() {
        worker = GlobalScope.launch(Dispatchers.Unconfined) {
            val mode = currentMode
            if (mode.isMonitor) {
                if (mode != Mode.MonitorRoot) {
                    // monitor may get rejected by SELinux enforcing
                    withContext(Dispatchers.IO) {
                        handleProcess(ProcessBuilder(Routing.IP, "monitor", monitoredObject))
                    }
                    if (destroyed) return@launch
                }
                try {
                    RootManager.use { server ->
                        // while we only need to use this server once, we need to also keep the server alive
                        handleChannel(server.create(ProcessListener(errorMatcher,
                            Routing.IP, "monitor", monitoredObject), this))
                    }
                } catch (_: CancellationException) {
                } catch (e: Exception) {
                    Timber.w(e)
                }
                if (destroyed) return@launch
                app.logEvent("ip_monitor_failure")
            }
            withContext(Dispatchers.IO) {
                var server: RootServer? = null
                try {
                    while (isActive) {
                        delay(1000)
                        server = work(server)
                    }
                } finally {
                    if (server != null) RootManager.release(server)
                }
            }
        }
        flushAsync()
    }

    /**
     * Possibly blocking. Should run in IO dispatcher or use [flushAsync].
     */
    suspend fun flush() = work(null)?.let {
        try {
            RootManager.release(it)
        } catch (e: Exception) {
            Timber.w(e)
        }
    }
    fun flushAsync() = GlobalScope.launch(Dispatchers.IO) { flush() }

    private suspend fun work(server: RootServer?): RootServer? {
        if (currentMode != Mode.PollRoot && currentMode != Mode.MonitorRoot) try {
            poll()
            return server
        } catch (e: IOException) {
            app.logEvent("ip_poll_failure")
            Timber.d(e)
        }
        var newServer = server
        try {
            val command = listOf(Routing.IP, monitoredObject)
            val result = (server ?: RootManager.acquire().also { newServer = it })
                    .execute(RoutingCommands.Process(command))
            result.check(command, false)
            val lines = result.out.lines()
            if (lines.any { errorMatcher.containsMatchIn(it) }) throw IOException(result.out)
            processLines(lines.asSequence())
        } catch (e: Exception) {
            app.logEvent("ip_su_poll_failure") { param("cause", e.message.toString()) }
            Timber.d(e)
        }
        return if (newServer?.active != false) newServer else {
            try {
                RootManager.release(newServer!!)
            } catch (e: Exception) {
                Timber.w(e)
            }
            null
        }
    }

    @Suppress("BlockingMethodInNonBlockingContext")
    private suspend fun poll() {
        val process = ProcessBuilder(Routing.IP, monitoredObject).apply {
            redirectErrorStream(true)
        }.start()
        process.waitFor()
        thread(name = "${javaClass.simpleName}-flush-error") {
            val err = process.errorStream.bufferedReader().readText()
            if (err.isNotBlank()) {
                Timber.e(err)
                Timber.i(FlushFailure())
                SmartSnackbar.make(R.string.noisy_su_failure).show()
            }
        }
        process.inputStream.bufferedReader().useLines {
            processLines(it.map { line ->
                if (errorMatcher.containsMatchIn(line)) throw IOException(line)
                line
            })
        }
    }

    fun destroy() {
        destroyed = true
        monitor?.destroy()
        worker.cancel()
    }
}
