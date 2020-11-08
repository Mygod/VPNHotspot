package be.mygod.vpnhotspot.net.monitor

import android.os.Build
import android.system.ErrnoException
import android.system.OsConstants
import androidx.core.content.edit
import be.mygod.librootkotlinx.RootServer
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.BuildConfig
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
        private val errorMatcher = ("(^Cannot bind netlink socket: |" +
                "Dump (was interrupted and may be inconsistent.|terminated)$)").toRegex()
        var currentMode: Mode
            get() {
                val isLegacy = Build.VERSION.SDK_INT < 30 || BuildConfig.TARGET_SDK < 30
                val defaultMode = if (isLegacy) @Suppress("DEPRECATION") {
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
    protected abstract fun processLine(line: String)
    protected abstract fun processLines(lines: Sequence<String>)

    @Volatile
    private var destroyed = false
    private var monitor: Process? = null
    private val worker = Job()

    private fun handleProcess(builder: ProcessBuilder) {
        val process = try {
            builder.start()
        } catch (e: IOException) {
            Timber.d(e)
            return
        }
        monitor = process
        val err = thread(name = "${javaClass.simpleName}-error") {
            try {
                process.errorStream.bufferedReader().forEachLine { Timber.e(it) }
            } catch (_: InterruptedIOException) { } catch (e: IOException) {
                if ((e.cause as? ErrnoException)?.errno != OsConstants.EBADF) Timber.w(e)
            }
        }
        try {
            process.inputStream.bufferedReader().forEachLine {
                if (errorMatcher.containsMatchIn(it)) {
                    Timber.w(it)
                    process.destroy()   // move on to next mode
                } else processLine(it)
            }
        } catch (_: InterruptedIOException) { } catch (e: IOException) {
            if ((e.cause as? ErrnoException)?.errno != OsConstants.EBADF) Timber.w(e)
        }
        err.join()
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
        thread(name = "${javaClass.simpleName}-input") {
            val mode = currentMode
            if (mode.isMonitor) {
                if (mode != Mode.MonitorRoot) {
                    // monitor may get rejected by SELinux enforcing
                    handleProcess(ProcessBuilder(Routing.IP, "monitor", monitoredObject))
                    if (destroyed) return@thread
                }
                try {
                    runBlocking(worker) {
                        RootManager.use { server ->
                            // while we only need to use this server once, we need to also keep the server alive
                            handleChannel(server.create(ProcessListener(errorMatcher, Routing.IP, "monitor", monitoredObject),
                                    this))
                        }
                    }
                } catch (_: CancellationException) {
                } catch (e: Exception) {
                    Timber.w(e)
                }
                if (destroyed) return@thread
                app.logEvent("ip_monitor_failure")
            }
            GlobalScope.launch(Dispatchers.IO + worker) {
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
        if (currentMode != Mode.PollRoot) try {
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

    private fun poll() {
        val process = ProcessBuilder(Routing.IP, monitoredObject)
                .redirectErrorStream(true)
                .start()
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
