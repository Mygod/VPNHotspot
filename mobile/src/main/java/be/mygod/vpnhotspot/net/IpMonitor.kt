package be.mygod.vpnhotspot.net

import android.util.Log
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.util.thread
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.crashlytics.android.Crashlytics
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

    private fun handleProcess(process: Process): Int {
        val err = thread("${javaClass.simpleName}-error") {
            try {
                process.errorStream.bufferedReader().forEachLine {
                    Crashlytics.log(Log.ERROR, javaClass.simpleName, it)
                }
            } catch (_: InterruptedIOException) { } catch (e: IOException) {
                e.printStackTrace()
                Crashlytics.logException(e)
            }
        }
        try {
            process.inputStream.bufferedReader().forEachLine(this::processLine)
        } catch (_: InterruptedIOException) { } catch (e: IOException) {
            e.printStackTrace()
            Crashlytics.logException(e)
        }
        err.join()
        process.waitFor()
        return process.exitValue()
    }

    init {
        thread("${javaClass.simpleName}-input") {
            // monitor may get rejected by SELinux
            if (handleProcess(ProcessBuilder("ip", "monitor", monitoredObject).start()) == 0) return@thread
            if (handleProcess(ProcessBuilder("su", "-c", "exec ip monitor $monitoredObject").start()) == 0)
                return@thread
            Crashlytics.log(Log.WARN, javaClass.simpleName, "Failed to set up monitor, switching to polling")
            Crashlytics.logException(MonitorFailure())
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
                Crashlytics.log(Log.ERROR, javaClass.simpleName, err)
                Crashlytics.logException(FlushFailure())
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
