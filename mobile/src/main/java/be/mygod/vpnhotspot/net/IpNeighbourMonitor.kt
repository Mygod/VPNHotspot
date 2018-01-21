package be.mygod.vpnhotspot.net

import android.os.Build
import android.os.Handler
import android.util.Log
import be.mygod.vpnhotspot.debugLog
import java.io.InterruptedIOException

class IpNeighbourMonitor private constructor() {
    companion object {
        private const val TAG = "IpNeighbourMonitor"
        private val callbacks = HashSet<Callback>()
        var instance: IpNeighbourMonitor? = null

        fun registerCallback(callback: Callback) {
            if (!callbacks.add(callback)) return
            var monitor = instance
            if (monitor == null) {
                monitor = IpNeighbourMonitor()
                instance = monitor
                monitor.flush()
            } else synchronized(monitor.neighbours) { callback.onIpNeighbourAvailable(monitor.neighbours) }
        }
        fun unregisterCallback(callback: Callback) {
            if (!callbacks.remove(callback) || callbacks.isNotEmpty()) return
            val monitor = instance ?: return
            instance = null
            monitor.monitor?.destroy()
        }

        /**
         * Wrapper for kotlin.concurrent.thread that silences uncaught exceptions.
         */
        private fun thread(name: String? = null, start: Boolean = true, isDaemon: Boolean = false,
                           contextClassLoader: ClassLoader? = null, priority: Int = -1, block: () -> Unit): Thread {
            val thread = kotlin.concurrent.thread(false, isDaemon, contextClassLoader, name, priority, block)
            thread.setUncaughtExceptionHandler { _, _ -> }
            if (start) thread.start()
            return thread
        }
    }

    interface Callback {
        fun onIpNeighbourAvailable(neighbours: Map<IpNeighbour, IpNeighbour.State>)
        fun postIpNeighbourAvailable() { }
    }

    private val handler = Handler()
    private var updatePosted = false
    val neighbours = HashMap<IpNeighbour, IpNeighbour.State>()
    /**
     * Using monitor requires using /proc/self/ns/net which would be problematic on Android 6.0+.
     *
     * Source: https://source.android.com/security/enhancements/enhancements60
     */
    private var monitor: Process? = null

    init {
        thread(name = TAG + "-input") {
            val monitor = (if (Build.VERSION.SDK_INT >= 23)
                ProcessBuilder("su", "-c", "ip", "-4", "monitor", "neigh") else
                ProcessBuilder("ip", "-4", "monitor", "neigh"))
                    .redirectErrorStream(true)
                    .start()
            this.monitor = monitor
            thread(name = TAG + "-error") {
                try {
                    monitor.errorStream.bufferedReader().forEachLine { Log.e(TAG, it) }
                } catch (ignore: InterruptedIOException) { }
            }
            try {
                monitor.inputStream.bufferedReader().forEachLine {
                    debugLog(TAG, it)
                    synchronized(neighbours) {
                        val (neighbour, state) = IpNeighbour.parse(it) ?: return@forEachLine
                        val changed = if (state == IpNeighbour.State.DELETING) neighbours.remove(neighbour) != null else
                            neighbours.put(neighbour, state) != state
                        if (changed) postUpdateLocked()
                    }
                }
                Log.w(TAG, if (Build.VERSION.SDK_INT >= 26 && monitor.isAlive) "monitor closed stdout" else
                    "monitor died unexpectedly")
            } catch (ignore: InterruptedIOException) { }
        }
    }

    fun flush() = thread(name = TAG + "-flush") {
        val process = ProcessBuilder("ip", "-4", "neigh")
                .redirectErrorStream(true)
                .start()
        process.waitFor()
        val err = process.errorStream.bufferedReader().readText()
        if (err.isNotBlank()) Log.e(TAG, err)
        process.inputStream.bufferedReader().useLines {
            synchronized(neighbours) {
                neighbours.clear()
                neighbours.putAll(it.map(IpNeighbour.Companion::parse).filterNotNull().toMap())
                postUpdateLocked()
            }
        }
    }

    private fun postUpdateLocked() {
        if (updatePosted || instance != this) return
        handler.post {
            synchronized(neighbours) {
                for (callback in callbacks) callback.onIpNeighbourAvailable(neighbours)
                updatePosted = false
            }
            for (callback in callbacks) callback.postIpNeighbourAvailable()
        }
        updatePosted = true
    }
}
