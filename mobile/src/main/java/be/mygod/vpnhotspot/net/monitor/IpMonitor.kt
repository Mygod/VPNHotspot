package be.mygod.vpnhotspot.net.monitor

import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.util.thread
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.io.IOException
import java.io.InterruptedIOException
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit

abstract class IpMonitor : Runnable {
    private class MonitorFailure : RuntimeException()
    private class FlushFailure : RuntimeException()
    protected abstract val monitoredObject: String
    protected abstract fun processLine(line: String)
    protected abstract fun processLines(lines: Sequence<String>)

    private var monitor: Process? = null
    private var pool: ScheduledExecutorService? = null

    private fun handleProcess(builder: ProcessBuilder): Boolean {
        val process = try {
            builder.start()
        } catch (e: IOException) {
            Timber.d(e)
            return false
        }
        monitor = process
        val err = thread("${javaClass.simpleName}-error") {
            try {
                process.errorStream.bufferedReader().forEachLine { Timber.e(it) }
            } catch (_: InterruptedIOException) { } catch (e: IOException) {
                Timber.w(e)
            }
        }
        try {
            process.inputStream.bufferedReader().forEachLine(this::processLine)
        } catch (_: InterruptedIOException) { } catch (e: IOException) {
            Timber.w(e)
        }
        err.join()
        process.waitFor()
        val result = process.exitValue()
        val success = result == 0 || result == 143 // SIGTERM
        if (!success) Timber.e("Monitor process exited with $result")
        return success
    }

    init {
        thread("${javaClass.simpleName}-input") {
            // monitor may get rejected by SELinux
            if (handleProcess(ProcessBuilder("ip", "monitor", monitoredObject))) return@thread
            if (handleProcess(ProcessBuilder("su", "-c", "exec ip monitor $monitoredObject"))) return@thread
            Timber.w("Failed to set up monitor, switching to polling")
            Timber.i(MonitorFailure())
            val pool = Executors.newScheduledThreadPool(1)
            pool.scheduleAtFixedRate(this, 1, 1, TimeUnit.SECONDS)
            this.pool = pool
        }
    }

    fun flush() = thread("${javaClass.simpleName}-flush") { run() }

    override fun run() {
        val process = ProcessBuilder("ip", monitoredObject)
                .redirectErrorStream(true)
                .start()
        process.waitFor()
        thread("${javaClass.simpleName}-flush-error") {
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
        val monitor = monitor
        if (monitor != null) thread("${javaClass.simpleName}-killer") { monitor.destroy() }
        pool?.shutdown()
    }
}
