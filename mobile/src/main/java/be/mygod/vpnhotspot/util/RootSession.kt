package be.mygod.vpnhotspot.util

import android.util.Log
import androidx.core.os.postDelayed
import be.mygod.vpnhotspot.App.Companion.app
import com.crashlytics.android.Crashlytics
import com.topjohnwu.superuser.Shell
import java.util.*
import java.util.concurrent.locks.ReentrantLock
import kotlin.collections.ArrayList
import kotlin.concurrent.withLock

class RootSession : AutoCloseable {
    companion object {
        private const val TAG = "RootSession"
        private const val INIT_CHECKPOINT = "$TAG initialized successfully"

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
    }

    class UnexpectedOutputException(msg: String) : RuntimeException(msg)
    private fun checkOutput(command: String, result: Shell.Result, out: Boolean = result.out.isNotEmpty(),
                            err: Boolean = stderr.isNotEmpty()) {
        if (result.isSuccess && !out && !err) return
        val msg = StringBuilder("$command exited with ${result.code}")
        if (out) result.out.forEach { msg.append("\n$it") }
        // TODO bug: https://github.com/topjohnwu/libsu/pull/23
        if (err) stderr.forEach { msg.append("\nE $it") }
        throw UnexpectedOutputException(msg.toString())
    }

    private val shell = Shell.newInstance("su")
    private val stdout = ArrayList<String>()
    private val stderr = ArrayList<String>()

    init {
        // check basic shell functionality very basically
        val result = execQuiet("echo $INIT_CHECKPOINT")
        checkOutput("echo", result, result.out.joinToString("\n").trim() != INIT_CHECKPOINT)
    }

    private val isAlive get() = shell.isAlive
    override fun close() {
        shell.close()
        if (instance == this) instance = null
    }
    private fun startTimeout() = app.handler.postDelayed(60 * 1000, this) { monitor.withLock { close() } }
    private fun haltTimeout() = app.handler.removeCallbacksAndMessages(this)

    /**
     * Don't care about the results, but still sync.
     */
    fun submit(command: String) {
        val result = execQuiet(command)
        if (result.code != 0) Crashlytics.log(Log.VERBOSE, TAG, "$command exited with ${result.code}")
        var msg = stderr.joinToString("\n").trim()
        if (msg.isNotEmpty()) Crashlytics.log(Log.VERBOSE, TAG, msg)
        msg = result.out.joinToString("\n").trim()
        if (msg.isNotEmpty()) Crashlytics.log(Log.VERBOSE, TAG, msg)
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

        fun exec(command: String, revert: String? = null) {
            if (revert != null) revertCommands.addFirst(revert) // add first just in case exec fails
            this@RootSession.exec(command)
        }
        fun execQuiet(command: String) = this@RootSession.execQuiet(command)

        fun commit() = unlock()

        fun revert() {
            if (revertCommands.isEmpty()) return
            val shell = if (monitor.isHeldByCurrentThread) this@RootSession else {
                monitor.lock()
                ensureInstance()
            }
            shell.haltTimeout()
            revertCommands.forEach { shell.submit(it) }
            revertCommands.clear()
            unlock()    // commit
        }
    }
}
