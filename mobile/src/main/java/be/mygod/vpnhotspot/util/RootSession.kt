package be.mygod.vpnhotspot.util

import android.os.Handler
import android.os.HandlerThread
import androidx.core.os.postDelayed
import com.topjohnwu.superuser.Shell
import timber.log.Timber
import java.util.*
import java.util.concurrent.TimeUnit
import java.util.concurrent.locks.ReentrantLock
import kotlin.collections.ArrayList
import kotlin.concurrent.withLock

class RootSession : AutoCloseable {
    companion object {
        private const val TAG = "RootSession"

        val handler = Handler(HandlerThread("$TAG-HandlerThread").apply { start() }.looper)

        private val monitor = ReentrantLock()
        private fun onUnlock() {
            if (monitor.holdCount == 1) instance?.startTimeout()
        }
        private fun unlock() {
            onUnlock()
            monitor.unlock()
        }

        private var instance: RootSession? = null
        private fun ensureInstance(): RootSession {
            var instance = instance
            if (instance == null || !instance.isAlive) instance = RootSession().also { RootSession.instance = it }
            return instance
        }
        fun <T> use(operation: (RootSession) -> T) = monitor.withLock {
            val instance = ensureInstance()
            instance.haltTimeout()
            operation(instance).also { onUnlock() }
        }
        fun beginTransaction(): Transaction {
            monitor.lock()
            val instance = try {
                ensureInstance()
            } catch (e: RuntimeException) {
                unlock()
                throw e
            }
            instance.haltTimeout()
            return instance.Transaction()
        }

        fun trimMemory() = monitor.withLock {
            val instance = instance ?: return
            instance.haltTimeout()
            instance.close()
        }
    }

    class UnexpectedOutputException(msg: String) : RuntimeException(msg)
    fun checkOutput(command: String, result: Shell.Result, out: Boolean = result.out.isNotEmpty(),
                    err: Boolean = result.err.isNotEmpty()): String {
        val msg = StringBuilder("$command exited with ${result.code}")
        if (out) result.out.forEach { msg.append("\n$it") }
        if (err) result.err.forEach { msg.append("\nE $it") }
        if (!result.isSuccess || out || err) throw UnexpectedOutputException(msg.toString()) else return msg.toString()
    }

    private val shell = Shell.newInstance("su")
    private val stdout = ArrayList<String>()
    private val stderr = ArrayList<String>()

    private val isAlive get() = shell.isAlive
    override fun close() {
        shell.close()
        if (instance == this) instance = null
    }
    private fun startTimeout() = handler.postDelayed(TimeUnit.MINUTES.toMillis(5), this) {
        monitor.withLock { close() }
    }
    private fun haltTimeout() = handler.removeCallbacksAndMessages(this)

    /**
     * Don't care about the results, but still sync.
     */
    fun submit(command: String) {
        val result = execQuiet(command)
        val err = result.err.joinToString("\n") { "E $it" }.trim()
        val out = result.out.joinToString("\n").trim()
        if (result.code != 0 || err.isNotEmpty() || out.isNotEmpty()) {
            Timber.v("$command exited with ${result.code}")
            if (err.isNotEmpty()) Timber.v(err)
            if (out.isNotEmpty()) Timber.v(out)
        }
    }

    fun execQuiet(command: String, redirect: Boolean = false): Shell.Result {
        stdout.clear()
        return shell.newJob().add(command).to(stdout, if (redirect) stdout else {
            stderr.clear()
            stderr
        }).exec()
    }
    fun exec(command: String) = checkOutput(command, execQuiet(command))
    fun execOutUnjoined(command: String): List<String> {
        val result = execQuiet(command)
        checkOutput(command, result, false)
        return result.out
    }
    fun execOut(command: String): String = execOutUnjoined(command).joinToString("\n")

    /**
     * This transaction is different from what you may have in mind since you can revert it after committing it.
     */
    inner class Transaction {
        private val revertCommands = LinkedList<String>()

        fun exec(command: String, revert: String? = null, wait: Boolean = false) {
            if (revert != null) revertCommands.addFirst(revert) // add first just in case exec fails
            if (wait) {
                val result = this@RootSession.execQuiet(command)
                val message = checkOutput(command, result, err = false)
                if (result.err.isNotEmpty()) Timber.i(message)
            } else this@RootSession.exec(command)
        }
        fun execQuiet(command: String) = this@RootSession.execQuiet(command)

        fun commit() = unlock()

        fun revert() {
            if (revertCommands.isEmpty()) return
            var locked = monitor.isHeldByCurrentThread
            try {
                val shell = if (locked) this@RootSession else {
                    monitor.lock()
                    locked = true
                    ensureInstance()
                }
                shell.haltTimeout()
                revertCommands.forEach { shell.submit(it) }
            } catch (e: RuntimeException) { // if revert fails, it should fail silently
                Timber.d(e)
            } finally {
                revertCommands.clear()
                if (locked) unlock()        // commit
            }
        }

        fun safeguard(work: Transaction.() -> Unit) = try {
            work()
            commit()
            this
        } catch (e: Exception) {
            revert()
            throw e
        }
    }
}
