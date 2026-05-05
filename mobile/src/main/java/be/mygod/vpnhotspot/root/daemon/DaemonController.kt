package be.mygod.vpnhotspot.root.daemon

import android.net.LocalServerSocket
import android.net.LocalSocket
import android.os.MessageQueue
import android.os.ParcelFileDescriptor
import android.os.Process
import android.system.ErrnoException
import android.system.OsConstants
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.io.ParcelFileDescriptorReadChannel
import be.mygod.vpnhotspot.io.drainLines
import be.mygod.vpnhotspot.io.isNonblocking
import be.mygod.vpnhotspot.io.openReadChannel
import be.mygod.vpnhotspot.io.openWriteChannel
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.util.Services
import dalvik.system.BaseDexClassLoader
import io.ktor.utils.io.ByteReadChannel
import io.ktor.utils.io.ByteWriteChannel
import io.ktor.utils.io.readLineTo
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import timber.log.Timber
import java.io.IOException
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlin.random.Random
import kotlin.time.Duration.Companion.seconds

object DaemonController {
    private const val BINARY_NAME = "vpnhotspotd"

    private val lock = Mutex()
    private val activeSessions = mutableSetOf<String>()
    private var socket: LocalSocket? = null
    private var input: ByteReadChannel? = null
    private var output: ByteWriteChannel? = null
    private var readerJob: Job? = null
    private val pendingReplies = ArrayDeque<CompletableDeferred<ByteArray>>()
    private var neighbourMonitorActive = false
    private var neighbourListener: (suspend (DaemonProtocol.NeighbourUpdate) -> Unit)? = null
    private var daemonStdioClosing = false
    private var daemonStdioEofReported = false
    private val logScope = CoroutineScope(Dispatchers.Main.immediate + SupervisorJob())
    private val stdoutLog = DaemonLog("stdout") { Timber.tag(BINARY_NAME).i(it) }
    private val stderrLog = DaemonLog("stderr") { Timber.tag(BINARY_NAME).e(it) }

    /**
     * Android 10 bionic supports direct linker execution of uncompressed, page-aligned zip entries:
     * https://android.googlesource.com/platform/bionic/+/android-10.0.0_r7/linker/linker_main.cpp#663
     * Android 10 DexPathList returns zip native-library paths only for stored entries:
     * https://android.googlesource.com/platform/libcore/+/android-10.0.0_r1/dalvik/src/main/java/dalvik/system/DexPathList.java#884
     */
    private val daemonCommand by lazy {
        val path = (app.classLoader as BaseDexClassLoader).findLibrary(BINARY_NAME) ?: error("Daemon binary missing")
        listOf(if (Process.is64Bit()) "/system/bin/linker64" else "/system/bin/linker", path)
    }

    suspend fun startSession(config: DaemonProtocol.SessionConfig): DaemonProtocol.SessionPorts {
        var leaseAdded = false
        lock.withLock {
            leaseAdded = activeSessions.add(config.downstream)
        }
        try {
            return DaemonProtocol.readPorts(request(DaemonProtocol.startSession(config)))
        } catch (e: DaemonProtocol.StatusException) {
            if (leaseAdded) lock.withLock {
                activeSessions.remove(config.downstream)
                maybeShutdownLocked()
            }
            throw e
        } catch (e: Exception) {
            lock.withLock { closeAndClearStateLocked() }
            throw e
        }
    }

    suspend fun replaceSession(config: DaemonProtocol.SessionConfig) {
        try {
            DaemonProtocol.readAck(request(DaemonProtocol.replaceSession(config)))
        } catch (e: DaemonProtocol.StatusException) {
            throw e
        } catch (e: Exception) {
            lock.withLock { closeAndClearStateLocked() }
            throw e
        }
    }

    suspend fun removeSession(
        downstream: String,
        removeMode: DaemonProtocol.RemoveMode = DaemonProtocol.RemoveMode.PreserveCleanup,
    ) {
        if (lock.withLock { output == null }) return
        try {
            DaemonProtocol.readAck(request(DaemonProtocol.removeSession(downstream, removeMode)))
        } catch (e: Exception) {
            lock.withLock { closeAndClearStateLocked() }
            if (e is CancellationException) throw e
            Timber.w(e)
        }
        lock.withLock {
            activeSessions.remove(downstream)
            maybeShutdownLocked()
        }
    }

    suspend fun readTrafficCounterLines() = try {
        DaemonProtocol.readTrafficCounterLines(request(DaemonProtocol.readTrafficCounters()))
    } catch (e: DaemonProtocol.StatusException) {
        throw e
    } catch (e: Exception) {
        lock.withLock { closeAndClearStateLocked() }
        throw e
    }

    suspend fun startNeighbourMonitor(listener: suspend (DaemonProtocol.NeighbourUpdate) -> Unit) {
        lock.withLock {
            neighbourMonitorActive = true
            neighbourListener = listener
        }
        try {
            DaemonProtocol.readAck(request(DaemonProtocol.startNeighbourMonitor()))
        } catch (e: Exception) {
            lock.withLock {
                neighbourMonitorActive = false
                if (neighbourListener === listener) neighbourListener = null
                if (e !is DaemonProtocol.StatusException) {
                    closeAndClearStateLocked()
                } else {
                    maybeShutdownLocked()
                }
            }
            throw e
        }
    }

    suspend fun stopNeighbourMonitor() {
        val shouldSend = lock.withLock {
            if (!neighbourMonitorActive) return
            neighbourMonitorActive = false
            neighbourListener = null
            output != null
        }
        if (shouldSend) try {
            DaemonProtocol.readAck(request(DaemonProtocol.stopNeighbourMonitor()))
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Timber.w(e)
        }
        lock.withLock { maybeShutdownLocked() }
    }

    suspend fun dumpNeighbours() = DaemonProtocol.readNeighbours(request(DaemonProtocol.dumpNeighbours()))

    suspend fun replaceStaticAddress(address: java.net.InetAddress, prefixLength: Int, dev: String) {
        DaemonProtocol.readAck(request(DaemonProtocol.staticAddress(
            DaemonProtocol.IpOperation.Replace, address, prefixLength, dev)))
    }

    suspend fun deleteStaticAddress(address: java.net.InetAddress, prefixLength: Int, dev: String) {
        DaemonProtocol.readAck(request(DaemonProtocol.staticAddress(
            DaemonProtocol.IpOperation.Delete, address, prefixLength, dev)))
    }

    suspend fun cleanRouting(ipv6NatPrefixSeed: String) {
        DaemonProtocol.readAck(request(DaemonProtocol.cleanRouting(ipv6NatPrefixSeed)))
    }

    private class DaemonStdioEofException(message: String) : IOException(message)

    private suspend fun ensureDaemonLocked() {
        if (socket != null) return
        daemonStdioClosing = false
        daemonStdioEofReported = false
        val socketName = "be.mygod.vpnhotspot.${Process.myPid()}.${Random.nextLong().toHexString()}"
        var stdout: ParcelFileDescriptor? = null
        var stderr: ParcelFileDescriptor? = null
        try {
            stdout = stdoutLog.openPipe(logScope)
            stderr = stderrLog.openPipe(logScope)
            LocalServerSocket(socketName).use { serverSocket ->
                RootManager.use { server ->
                    try {
                        server.execute(RunDaemon(
                            daemonCommand,
                            socketName,
                            stdout!!,
                            stderr!!,
                        ))
                    } finally {
                        try {
                            stdout?.close()
                        } catch (_: IOException) { }
                        try {
                            stderr?.close()
                        } catch (_: IOException) { }
                        stdout = null
                        stderr = null
                    }
                }
                acceptLocked(serverSocket).also {
                    socket = it
                    input = ParcelFileDescriptor.dup(it.fileDescriptor).openReadChannel(Services.mainHandler.looper)
                    output = ParcelFileDescriptor.dup(it.fileDescriptor).openWriteChannel(Services.mainHandler.looper)
                    startReaderLocked(input!!)
                }
            }
        } catch (e: Exception) {
            try {
                stdout?.close()
            } catch (_: IOException) { }
            try {
                stderr?.close()
            } catch (_: IOException) { }
            closeConnectionLocked()
            throw e
        }
    }

    private suspend fun acceptLocked(serverSocket: LocalServerSocket): LocalSocket {
        val descriptor = serverSocket.fileDescriptor
        descriptor.isNonblocking = true
        return withTimeoutOrNull(10.seconds) {
            suspendCancellableCoroutine { continuation ->
                val events = MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT or
                        MessageQueue.OnFileDescriptorEventListener.EVENT_ERROR
                val active = AtomicBoolean(true)
                val messageQueue = Services.mainHandler.looper.queue
                val listener = MessageQueue.OnFileDescriptorEventListener { _, receivedEvents ->
                    if (!active.get() || !continuation.isActive) return@OnFileDescriptorEventListener 0
                    if ((receivedEvents and MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT) == 0) {
                        if (active.compareAndSet(true, false) && continuation.isActive) {
                            continuation.resumeWithException(IOException("Unexpected $BINARY_NAME listener event " +
                                    receivedEvents))
                        }
                        return@OnFileDescriptorEventListener 0
                    }
                    try {
                        val accepted = serverSocket.accept()
                        if (active.compareAndSet(true, false) && continuation.isActive) {
                            continuation.resume(accepted)
                        } else {
                            accepted.close()
                        }
                        0
                    } catch (e: IOException) {
                        if ((e.cause as? ErrnoException)?.errno == OsConstants.EAGAIN) {
                            events
                        } else {
                            if (active.compareAndSet(true, false) && continuation.isActive) {
                                continuation.resumeWithException(e)
                            }
                            0
                        }
                    }
                }
                messageQueue.addOnFileDescriptorEventListener(descriptor, events, listener)
                continuation.invokeOnCancellation {
                    if (active.compareAndSet(true, false)) messageQueue.removeOnFileDescriptorEventListener(descriptor)
                }
                if (!continuation.isActive && active.compareAndSet(true, false)) {
                    messageQueue.removeOnFileDescriptorEventListener(descriptor)
                }
            }
        } ?: throw IOException("Timed out waiting for $BINARY_NAME to connect")
    }

    private suspend fun request(packet: ByteArray): ByteArray {
        val reply = CompletableDeferred<ByteArray>()
        lock.withLock {
            try {
                ensureDaemonLocked()
                pendingReplies.addLast(reply)
                writePacketLocked(packet)
            } catch (e: Exception) {
                pendingReplies.remove(reply)
                closeConnectionLocked()
                throw e
            }
        }
        return reply.await()
    }

    private fun startReaderLocked(input: ByteReadChannel) {
        readerJob = logScope.launch {
            try {
                while (currentCoroutineContext().isActive) when (val frame = DaemonProtocol.readFrame(
                    DaemonIpc.readFrame(input))) {
                    is DaemonProtocol.Frame.Reply -> lock.withLock {
                        val reply = if (pendingReplies.isEmpty()) null else pendingReplies.removeFirst()
                        if (reply == null) {
                            Timber.w("Unexpected $BINARY_NAME reply")
                        } else {
                            reply.complete(frame.packet)
                            maybeShutdownLocked()
                        }
                    }
                    is DaemonProtocol.Frame.Neighbours -> {
                        val listener = lock.withLock { neighbourListener }
                        if (listener == null) {
                            Timber.d("Dropping neighbour update without listener")
                        } else try {
                            listener(frame.update)
                        } catch (e: CancellationException) {
                            throw e
                        } catch (e: Exception) {
                            Timber.w(e)
                        }
                    }
                }
            } catch (_: CancellationException) {
            } catch (e: Exception) {
                lock.withLock {
                    if (!daemonStdioClosing) Timber.w(e)
                    closeAndClearStateLocked(cancelReader = false, cause = e)
                }
            }
        }
    }

    private fun completePendingRepliesLocked(e: Throwable) {
        while (pendingReplies.isNotEmpty()) pendingReplies.removeFirst().completeExceptionally(e)
    }

    private suspend fun maybeShutdownLocked() {
        if (output == null || pendingReplies.isNotEmpty() || activeSessions.isNotEmpty() ||
                neighbourMonitorActive) return
        try {
            writePacketLocked(DaemonProtocol.shutdown(DaemonProtocol.RemoveMode.PreserveCleanup))
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Timber.w(e)
        } finally {
            closeConnectionLocked()
        }
    }

    private suspend fun closeAndClearStateLocked(
        cancelReader: Boolean = true,
        cause: Throwable? = null,
    ) {
        if (cause != null) completePendingRepliesLocked(cause)
        closeConnectionLocked(cancelReader)
        activeSessions.clear()
        neighbourMonitorActive = false
        neighbourListener = null
    }

    private suspend fun writePacketLocked(packet: ByteArray) {
        DaemonIpc.writeFrame(output!!, packet)
    }

    private suspend fun closeConnectionLocked(cancelReader: Boolean = true) = withContext(NonCancellable) {
        daemonStdioClosing = true
        completePendingRepliesLocked(IOException("$BINARY_NAME connection closed"))
        if (cancelReader) readerJob?.cancel()
        readerJob = null
        input?.cancel(null)
        output?.cancel(null)
        try {
            socket?.close()
        } catch (_: IOException) { }
        withContext(Dispatchers.Main.immediate) {
            stdoutLog.close()
            stderrLog.close()
        }
        input = null
        output = null
        socket = null
    }

    private class DaemonLog(private val stream: String, private val log: (String) -> Unit) {
        private var channel: ParcelFileDescriptorReadChannel? = null
        private val line = StringBuilder()
        private var job: Job? = null

        fun openPipe(scope: CoroutineScope): ParcelFileDescriptor {
            val pipe = ParcelFileDescriptor.createPipe()
            var readEnd: ParcelFileDescriptor? = pipe[0]
            var writeEnd: ParcelFileDescriptor? = pipe[1]
            try {
                val channel = ParcelFileDescriptorReadChannel(readEnd!!, Services.mainHandler.looper)
                this.channel = channel
                readEnd = null
                job = scope.launch {
                    try {
                        while (channel.readLineTo(line) >= 0) {
                            log(line.toString())
                            line.clear()
                        }
                        if (line.isNotEmpty()) {
                            log(line.toString())
                            line.clear()
                        }
                        lock.withLock {
                            if (!daemonStdioClosing &&
                                    (activeSessions.isNotEmpty() || neighbourMonitorActive ||
                                            pendingReplies.isNotEmpty()) &&
                                    !daemonStdioEofReported) {
                                daemonStdioEofReported = true
                                Timber.w(DaemonStdioEofException("$BINARY_NAME $stream EOF " +
                                        "activeSessions=${activeSessions.size} " +
                                        "neighbourMonitor=$neighbourMonitorActive " +
                                        "pendingReplies=${pendingReplies.size}"))
                            }
                        }
                    } catch (e: ErrnoException) {
                        if (e.errno != OsConstants.EBADF) Timber.w(e)
                    }
                }
                return writeEnd!!.also { writeEnd = null }
            } finally {
                try {
                    readEnd?.close()
                } catch (_: IOException) { }
                try {
                    writeEnd?.close()
                } catch (_: IOException) { }
            }
        }

        suspend fun close() {
            channel?.let {
                try {
                    it.drain()
                } catch (e: ErrnoException) {
                    if (e.errno != OsConstants.EBADF) Timber.w(e)
                }
            }
            job?.cancelAndJoin()
            job = null
            channel?.let {
                try {
                    it.drainLines(line, flushPartial = true, block = log)
                } catch (e: ErrnoException) {
                    if (e.errno != OsConstants.EBADF) Timber.w(e)
                } finally {
                    it.cancel(null)
                }
            }
            channel = null
        }
    }
}
