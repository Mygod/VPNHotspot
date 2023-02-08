package be.mygod.vpnhotspot.util

import be.mygod.librootkotlinx.RootServer
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.RoutingCommands
import kotlinx.coroutines.runBlocking
import timber.log.Timber
import java.util.*
import java.util.concurrent.locks.ReentrantLock
import kotlin.concurrent.withLock

class RootSession : AutoCloseable {
    companion object {
        private val monitor = ReentrantLock()

        fun <T> use(operation: (RootSession) -> T) = monitor.withLock { operation(RootSession()) }
        fun beginTransaction(): Transaction {
            monitor.lock()
            val instance = try {
                RootSession()
            } catch (e: Exception) {
                monitor.unlock()
                throw e
            }
            return instance.Transaction()
        }
    }

    private var server: RootServer? = runBlocking { RootManager.acquire() }
    override fun close() {
        server?.let { runBlocking { RootManager.release(it) } }
        server = null
    }

    /**
     * Don't care about the results, but still sync.
     */
    fun submit(command: String) = execQuiet(command).message(listOf(command))?.let { Timber.v(it) }

    fun execQuiet(command: String, redirect: Boolean = false) = runBlocking {
        server!!.execute(RoutingCommands.Process(listOf("sh", "-c", command), redirect))
    }
    fun exec(command: String) = execQuiet(command).check(listOf(command))

    /**
     * This transaction is different from what you may have in mind since you can revert it after committing it.
     */
    inner class Transaction {
        private val revertCommands = LinkedList<String>()

        fun exec(command: String, revert: String? = null) = execQuiet(command, revert).check(listOf(command))
        fun execQuiet(command: String, revert: String? = null): RoutingCommands.ProcessResult {
            if (revert != null) revertCommands.addFirst(revert) // add first just in case exec fails
            return this@RootSession.execQuiet(command)
        }

        fun commit() = monitor.unlock()

        fun revert() {
            if (revertCommands.isEmpty()) return
            var locked = monitor.isHeldByCurrentThread
            try {
                val shell = if (locked) this@RootSession else {
                    monitor.lock()
                    locked = true
                    RootSession()
                }
                revertCommands.forEach { shell.submit(it) }
            } catch (e: Exception) {            // if revert fails, it should fail silently
                Timber.d(e)
            } finally {
                revertCommands.clear()
                if (locked) monitor.unlock()    // commit
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
