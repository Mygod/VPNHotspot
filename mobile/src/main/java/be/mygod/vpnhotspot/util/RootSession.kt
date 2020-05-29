package be.mygod.vpnhotspot.util

import android.os.Looper
import androidx.annotation.WorkerThread
import com.topjohnwu.superuser.Shell
import kotlinx.coroutines.*
import timber.log.Timber
import java.util.*
import java.util.concurrent.TimeUnit
import java.util.concurrent.locks.ReentrantLock
import kotlin.collections.ArrayList
import kotlin.concurrent.withLock

class RootSession : AutoCloseable {
    companion object {
        private val monitor = ReentrantLock()
        private fun onUnlock() {
            if (monitor.holdCount == 1) instance?.startTimeoutLocked()
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
            instance.haltTimeoutLocked()
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
            instance.haltTimeoutLocked()
            return instance.Transaction()
        }

        @WorkerThread
        fun trimMemory() = monitor.withLock {
            val instance = instance ?: return
            instance.haltTimeoutLocked()
            instance.close()
        }

        fun checkOutput(command: String, result: Shell.Result, out: Boolean = result.out.isNotEmpty(),
                        err: Boolean = result.err.isNotEmpty()): String {
            val msg = StringBuilder("$command exited with ${result.code}")
            if (out) result.out.forEach { msg.append("\n$it") }
            if (err) result.err.forEach { msg.append("\nE $it") }
            if (!result.isSuccess || out || err) throw UnexpectedOutputException(msg.toString(), result)
            return msg.toString()
        }
    }

    class UnexpectedOutputException(msg: String, val result: Shell.Result) : RuntimeException(msg)

    init {
        check(Looper.getMainLooper().thread != Thread.currentThread()) {
            "Unable to initialize shell in main thread" // https://github.com/topjohnwu/libsu/issues/33
        }
    }

    private val shell = Shell.newInstance("su")
    private val stdout = ArrayList<String>()
    private val stderr = ArrayList<String>()

    private val isAlive get() = shell.isAlive
    override fun close() {
        shell.close()
        if (instance == this) instance = null
    }

    private var timeoutJob: Job? = null
    private fun startTimeoutLocked() {
        check(timeoutJob == null)
        timeoutJob = GlobalScope.launch(start = CoroutineStart.UNDISPATCHED) {
            delay(TimeUnit.MINUTES.toMillis(5))
            monitor.withLock {
                close()
                timeoutJob = null
            }
        }
    }
    private fun haltTimeoutLocked() {
        timeoutJob?.cancel()
        timeoutJob = null
    }

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
    fun execOut(command: String): String {
        val result = execQuiet(command)
        checkOutput(command, result, false)
        return result.out.joinToString("\n")
    }

    /**
     * This transaction is different from what you may have in mind since you can revert it after committing it.
     */
    inner class Transaction {
        private val revertCommands = LinkedList<String>()

        fun exec(command: String, revert: String? = null) = checkOutput(command, execQuiet(command, revert))
        fun execQuiet(command: String, revert: String? = null): Shell.Result {
            if (revert != null) revertCommands.addFirst(revert) // add first just in case exec fails
            return this@RootSession.execQuiet(command)
        }

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
                shell.haltTimeoutLocked()
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
