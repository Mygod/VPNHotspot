package be.mygod.vpnhotspot.net

import android.os.Handler
import android.util.Log
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.util.debugLog
import be.mygod.vpnhotspot.util.thread
import java.io.InterruptedIOException
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledExecutorService
import java.util.concurrent.TimeUnit

class IpNeighbourMonitor private constructor() : Runnable {
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
                callback.onIpNeighbourAvailable(synchronized(monitor.neighbours) { monitor.neighbours.values.toList() })
            }
        }
        fun unregisterCallback(callback: Callback) {
            if (!callbacks.remove(callback) || callbacks.isNotEmpty()) return
            instance?.destroy()
            instance = null
        }
    }

    interface Callback {
        fun onIpNeighbourAvailable(neighbours: List<IpNeighbour>)
    }

    private val handler = Handler()
    private var updatePosted = false
    val neighbours = HashMap<String, IpNeighbour>()
    private var monitor: Process? = null
    private var pool: ScheduledExecutorService? = null

    init {
        thread("$TAG-input") {
            // monitor may get rejected by SELinux
            val monitor = ProcessBuilder("sh", "-c", "ip monitor neigh || su -c 'ip monitor neigh'")
                    .redirectErrorStream(true)
                    .start()
            this.monitor = monitor
            thread("$TAG-error") {
                try {
                    monitor.errorStream.bufferedReader().forEachLine { Log.e(TAG, it) }
                } catch (ignore: InterruptedIOException) {
                }
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
                if (monitor.exitValue() == 0) return@thread
                Log.w(TAG, "Failed to set up monitor, switching to polling")
                val pool = Executors.newScheduledThreadPool(1)
                pool.scheduleAtFixedRate(this, 1, 1, TimeUnit.SECONDS)
                this.pool = pool
            } catch (ignore: InterruptedIOException) {
            }
        }
    }

    fun flush() = thread("$TAG-flush") { run() }

    override fun run() {
        val process = ProcessBuilder("ip", "neigh")
                .redirectErrorStream(true)
                .start()
        process.waitFor()
        thread("$TAG-flush-error") {
            val err = process.errorStream.bufferedReader().readText()
            if (err.isNotBlank()) {
                Log.e(TAG, err)
                app.toast(R.string.noisy_su_failure)
            }
        }
        process.inputStream.bufferedReader().useLines {
            synchronized(neighbours) {
                neighbours.clear()
                neighbours.putAll(it
                        .map(IpNeighbour.Companion::parse)
                        .filterNotNull()
                        .filter { it.state != IpNeighbour.State.DELETING }  // skip entries without lladdr
                        .associateBy { it.ip })
                postUpdateLocked()
            }
        }
    }

    private fun postUpdateLocked() {
        if (updatePosted || instance != this) return
        handler.post {
            val neighbours = synchronized(neighbours) {
                updatePosted = false
                neighbours.values.toList()
            }
            for (callback in callbacks) callback.onIpNeighbourAvailable(neighbours)
        }
        updatePosted = true
    }

    fun destroy() {
        val monitor = monitor
        if (monitor != null) thread("$TAG-killer") { monitor.destroy() }
        pool?.shutdown()
    }
}
