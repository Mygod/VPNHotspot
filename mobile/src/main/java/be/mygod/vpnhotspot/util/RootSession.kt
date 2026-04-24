package be.mygod.vpnhotspot.util

import be.mygod.librootkotlinx.RootServer
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.RoutingCommands
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.withContext
import timber.log.Timber
import java.util.*

class RootSession private constructor(private var server: RootServer?) {
    companion object {
        private val monitor = Mutex()

        private suspend fun create() = RootSession(RootManager.acquire())

        suspend fun <T> use(operation: suspend (RootSession) -> T): T {
            monitor.lock()
            val session = try {
                create()
            } catch (e: Throwable) {
                monitor.unlock()
                throw e
            }
            try {
                return operation(session)
            } finally {
                try {
                    session.close()
                } finally {
                    monitor.unlock()
                }
            }
        }
        suspend fun beginTransaction(): Transaction {
            monitor.lock()
            return try {
                create().Transaction()
            } catch (e: Throwable) {
                monitor.unlock()
                throw e
            }
        }
    }

    private suspend fun close() {
        server?.let { RootManager.release(it) }
        server = null
    }

    /**
     * Don't care about the results, but still sync.
     */
    suspend fun submit(command: String) = execQuiet(command).message(listOf(command))?.let { Timber.v(it) }

    suspend fun execQuiet(command: String, redirect: Boolean = false) =
        server!!.execute(RoutingCommands.Process(listOf("sh", "-c", command), redirect))
    suspend fun exec(command: String) = execQuiet(command).check(listOf(command))

    /**
     * This transaction is different from what you may have in mind since you can revert it after committing it.
     */
    inner class Transaction {
        private val revertCommands = LinkedList<String>()
        private var locked = true

        suspend fun exec(command: String, revert: String? = null) = execQuiet(command, revert).check(listOf(command))
        suspend fun execQuiet(command: String, revert: String? = null): RoutingCommands.ProcessResult {
            if (revert != null) revertCommands.addFirst(revert) // add first just in case exec fails
            return this@RootSession.execQuiet(command)
        }

        suspend fun commit() {
            check(locked)
            locked = false
            try {
                this@RootSession.close()
            } finally {
                monitor.unlock()
            }
        }

        suspend fun revert() = withContext(NonCancellable) {
            val wasLocked = locked
            var shell: RootSession? = null
            var lockAcquired = false
            try {
                if (revertCommands.isEmpty()) return@withContext
                val currentShell = if (wasLocked) this@RootSession else {
                    monitor.lock()
                    lockAcquired = true
                    create()
                }
                shell = currentShell
                revertCommands.forEach { currentShell.submit(it) }
            } catch (e: Exception) {            // if revert fails, it should fail silently
                Timber.d(e)
            } finally {
                revertCommands.clear()
                if (wasLocked) {
                    locked = false
                    try {
                        this@RootSession.close()
                    } finally {
                        monitor.unlock()        // commit
                    }
                } else if (lockAcquired) {
                    try {
                        shell?.close()
                    } finally {
                        monitor.unlock()        // commit
                    }
                }
            }
        }

        suspend fun safeguard(work: suspend Transaction.() -> Unit) = try {
            work()
            commit()
            this
        } catch (e: Exception) {
            revert()
            throw e
        }
    }
}
