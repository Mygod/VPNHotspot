package be.mygod.librootkotlinx

import kotlinx.coroutines.*
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import java.util.concurrent.TimeUnit
import kotlin.coroutines.CoroutineContext

/**
 * This object manages creation of [RootServer] and times them out automagically, with default timeout of 5 minutes.
 */
abstract class RootSession {
    protected abstract suspend fun initServer(server: RootServer)
    /**
     * Timeout to close [RootServer] in milliseconds.
     */
    protected open val timeout get() = TimeUnit.MINUTES.toMillis(5)
    protected open val timeoutContext: CoroutineContext get() = Dispatchers.Default

    private val mutex = Mutex()
    private var server: RootServer? = null
    private var timeoutJob: Job? = null
    private var usersCount = 0L
    private var closePending = false

    private suspend fun ensureServerLocked(): RootServer {
        server?.let {
            if (it.active) return it
            usersCount = 0
            closeLocked()
        }
        check(usersCount == 0L) { "Unexpected $server, $usersCount" }
        val server = RootServer()
        try {
            initServer(server)
            this.server = server
            return server
        } catch (e: Throwable) {
            try {
                server.close()
            } catch (eClose: Throwable) {
                e.addSuppressed(eClose)
            }
            throw e
        }
    }

    private suspend fun closeLocked() {
        closePending = false
        val server = server
        this.server = null
        server?.close()
    }
    private fun startTimeoutLocked() {
        check(timeoutJob == null)
        timeoutJob = GlobalScope.launch(timeoutContext, CoroutineStart.UNDISPATCHED) {
            delay(timeout)
            mutex.withLock {
                check(usersCount == 0L)
                timeoutJob = null
                closeLocked()
            }
        }
    }
    private fun haltTimeoutLocked() {
        timeoutJob?.cancel()
        timeoutJob = null
    }

    suspend fun acquire() = withContext(NonCancellable) {
        mutex.withLock {
            haltTimeoutLocked()
            closePending = false
            ensureServerLocked().also { ++usersCount }
        }
    }
    suspend fun release(server: RootServer) = withContext(NonCancellable) {
        mutex.withLock {
            if (this@RootSession.server != server) return@withLock  // outdated reference
            require(usersCount > 0)
            when {
                !server.active -> {
                    usersCount = 0
                    closeLocked()
                    return@withLock
                }
                --usersCount > 0L -> return@withLock
                closePending -> closeLocked()
                else -> startTimeoutLocked()
            }
        }
    }
    suspend inline fun <T> use(block: (RootServer) -> T): T {
        val server = acquire()
        try {
            return block(server)
        } finally {
            release(server)
        }
    }

    suspend fun closeExisting() = mutex.withLock {
        if (usersCount > 0) closePending = true else {
            haltTimeoutLocked()
            closeLocked()
        }
    }
}
