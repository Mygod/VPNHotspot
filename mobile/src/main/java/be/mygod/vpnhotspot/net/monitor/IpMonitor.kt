package be.mygod.vpnhotspot.net.monitor

import android.system.ErrnoException
import android.system.OsConstants
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.DebugHelper
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.io.IOException
import java.io.InterruptedIOException
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread

abstract class IpMonitor : Runnable {
    companion object {
        const val KEY = "service.ipMonitor"
    }

    enum class Mode {
        Monitor, MonitorRoot, Poll
    }

    private class FlushFailure : RuntimeException()
    protected abstract val monitoredObject: String
    protected abstract fun processLine(line: String)
    protected abstract fun processLines(lines: Sequence<String>)

    @Volatile
    private var destroyed = false
    private var monitor: Process? = null
    private var pool: ScheduledExecutorService? = null

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
                // https://android.googlesource.com/platform/external/iproute2/+/7f7a711/lib/libnetlink.c#493
                if (it.endsWith("Dump was interrupted and may be inconsistent.")) {
                    Timber.w(it)
                    process.destroy()   // move on to next mode
                } else processLine(it)
            }
        } catch (_: InterruptedIOException) { } catch (e: IOException) {
            if ((e.cause as? ErrnoException)?.errno != OsConstants.EBADF) Timber.w(e)
        }
        err.join()
        process.waitFor()
        DebugHelper.log("IpMonitor", "Monitor process exited with ${process.exitValue()}")
    }

    init {
        thread(name = "${javaClass.simpleName}-input") {
            val mode = Mode.valueOf(app.pref.getString(KEY, Mode.Poll.toString()) ?: "")
            if (mode != Mode.Poll) {
                if (mode != Mode.MonitorRoot) {
                    // monitor may get rejected by SELinux enforcing
                    handleProcess(ProcessBuilder("ip", "monitor", monitoredObject))
                    if (destroyed) return@thread
                }
                handleProcess(ProcessBuilder("su", "-c", "exec ip monitor $monitoredObject"))
                if (destroyed) return@thread
                DebugHelper.logEvent("ip_monitor_failure")
            }
            val pool = Executors.newScheduledThreadPool(1)
            pool.scheduleAtFixedRate(this, 1, 1, TimeUnit.SECONDS)
            this.pool = pool
        }
    }

    fun flush() = thread(name = "${javaClass.simpleName}-flush") { run() }

    override fun run() {
        val process = ProcessBuilder("ip", monitoredObject)
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
        process.inputStream.bufferedReader().useLines(this::processLines)
    }

    fun destroy() {
        destroyed = true
        monitor?.destroy()
        pool?.shutdown()
    }
}
