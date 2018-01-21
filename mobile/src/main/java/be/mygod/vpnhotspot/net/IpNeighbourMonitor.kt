package be.mygod.vpnhotspot.net

import android.os.Handler
import android.util.Log
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
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
            } else {
                synchronized(monitor.neighbours) { callback.onIpNeighbourAvailable(monitor.neighbours) }
                callback.postIpNeighbourAvailable()
            }
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
            thread.setUncaughtExceptionHandler { _, _ -> app.toast(R.string.noisy_su_failure) }
            if (start) thread.start()
            return thread
        }
    }

    interface Callback {
        fun onIpNeighbourAvailable(neighbours: Map<String, IpNeighbour>)
        fun postIpNeighbourAvailable()
    }

    private val handler = Handler()
    private var updatePosted = false
    val neighbours = HashMap<String, IpNeighbour>()
    private var monitor: Process? = null

    init {
        thread(name = TAG + "-input") {
            // monitor may get rejected by SELinux
            val monitor = ProcessBuilder("sh", "-c", "ip -4 monitor neigh || su -c ip -4 monitor neigh")
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
                    synchronized(neighbours) {
                        val neighbour = IpNeighbour.parse(it) ?: return@forEachLine
                        debugLog(TAG, it)
                        val changed = if (neighbour.state == IpNeighbour.State.DELETING)
                            neighbours.remove(neighbour.ip) != null
                        else neighbours.put(neighbour.ip, neighbour) != neighbour
                        if (changed) postUpdateLocked()
                    }
                }
                monitor.waitFor()
                if (monitor.exitValue() != 0) app.toast(R.string.noisy_su_failure)
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
                neighbours.putAll(it.map(IpNeighbour.Companion::parse).filterNotNull().associateBy { it.ip })
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
