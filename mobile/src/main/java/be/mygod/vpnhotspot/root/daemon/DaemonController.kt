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
import be.mygod.vpnhotspot.widget.SmartSnackbar
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
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.ClosedReceiveChannelException
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import timber.log.Timber
import java.io.EOFException
import java.io.IOException
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlin.random.Random
import kotlin.time.Duration.Companion.seconds

object DaemonController {
    private const val BINARY_NAME = "vpnhotspotd"

    private val lock = Mutex()
    private var socket: LocalSocket? = null
    private var input: ByteReadChannel? = null
    private var output: ByteWriteChannel? = null
    private var readerJob: Job? = null
    private var nextCallId = 1L
    private val calls = mutableMapOf<Long, Call>()
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

    class SessionCall(val id: Long, val closed: Flow<Unit>) {
        suspend fun close() = closeEventCall(id)
    }

    suspend fun startSession(config: DaemonProtocol.SessionConfig): SessionCall {
        val call = eventCall(DaemonProtocol.startSession(config))
        try {
            DaemonProtocol.readAck(receiveEvent(call))
        } catch (e: Exception) {
            withContext(NonCancellable) { closeEventCall(call.id) }
            throw e
        }
        return SessionCall(call.id, flow {
            eventFlow(call, cancelOnClose = false).collect { packet ->
                throw IOException("Unexpected $BINARY_NAME session event payload length ${packet.size}")
            }
        })
    }

    suspend fun replaceSession(sessionId: Long, config: DaemonProtocol.SessionConfig) {
        try {
            DaemonProtocol.readAck(request(DaemonProtocol.replaceSession(sessionId, config)))
        } catch (e: DaemonTransport.DaemonException) {
            throw e
        } catch (e: Exception) {
            lock.withLock { closeAndClearStateLocked() }
            throw e
        }
    }

    suspend fun readTrafficCounterLines() = try {
        DaemonProtocol.readTrafficCounterLines(request(DaemonProtocol.readTrafficCounters()))
    } catch (e: DaemonTransport.DaemonException) {
        throw e
    } catch (e: Exception) {
        lock.withLock { closeAndClearStateLocked() }
        throw e
    }

    fun neighbourMonitor(): Flow<List<DaemonProtocol.NeighbourDelta>> = flow {
        eventFlow(eventCall(DaemonProtocol.startNeighbourMonitor())).collect {
            emit(DaemonProtocol.readNeighbourDeltas(it))
        }
    }

    suspend fun replaceStaticAddresses(dev: String, addresses: List<Pair<java.net.InetAddress, Int>>) {
        DaemonProtocol.readAck(request(DaemonProtocol.replaceStaticAddresses(dev, addresses)))
    }

    suspend fun deleteStaticAddresses(dev: String) {
        DaemonProtocol.readAck(request(DaemonProtocol.deleteStaticAddresses(dev)))
    }

    suspend fun cleanRouting(ipv6NatPrefixSeed: String) {
        DaemonProtocol.readAck(request(DaemonProtocol.cleanRouting(ipv6NatPrefixSeed)))
    }

    private sealed class Call {
        class OneShot(val reply: CompletableDeferred<ByteArray>) : Call()
        class Event(val channel: Channel<ByteArray>) : Call()
    }

    private class EventCall(val id: Long, val channel: Channel<ByteArray>)

    private class DaemonStdioEofException(message: String) : EOFException(message)

    private suspend fun ensureDaemonLocked() {
        if (socket != null) return
        Timber.d("Starting $BINARY_NAME")
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
                        server.execute(RunDaemon(daemonCommand, socketName, stdout!!, stderr!!))
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
                    Timber.d("Started $BINARY_NAME")
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

    private suspend fun request(command: DaemonProtocol.Command): ByteArray {
        var id = 0L
        val reply = lock.withLock {
            id = nextCallIdLocked()
            val reply = CompletableDeferred<ByteArray>()
            try {
                ensureDaemonLocked()
                calls[id] = Call.OneShot(reply)
                writePacketLocked(DaemonTransport.call(id, command.packet))
                Timber.d("Sent #$id: $command")
            } catch (e: Exception) {
                calls.remove(id)
                closeConnectionLocked()
                throw e
            }
            reply
        }
        try {
            return reply.await()
        } catch (e: DaemonTransport.DaemonException) {
            throw e.withCurrentTrace()
        } catch (e: CancellationException) {
            withContext(NonCancellable) {
                lock.withLock {
                    cancelCallLocked(id)
                    maybeShutdownLocked()
                }
            }
            throw e
        }
    }

    private suspend fun eventCall(command: DaemonProtocol.Command): EventCall {
        val channel = Channel<ByteArray>(Channel.UNLIMITED)
        val id = lock.withLock {
            val id = nextCallIdLocked()
            try {
                ensureDaemonLocked()
                calls[id] = Call.Event(channel)
                writePacketLocked(DaemonTransport.call(id, command.packet))
                Timber.d("Sent #$id: $command")
                id
            } catch (e: Exception) {
                calls.remove(id)
                closeConnectionLocked()
                throw e
            }
        }
        return EventCall(id, channel)
    }

    private fun eventFlow(call: EventCall, cancelOnClose: Boolean = true) = flow {
        try {
            for (event in call.channel) emit(event)
        } catch (e: DaemonTransport.DaemonException) {
            throw e.withCurrentTrace()
        } finally {
            if (cancelOnClose) withContext(NonCancellable) { closeEventCall(call.id) }
        }
    }.buffer(Channel.UNLIMITED)

    private suspend fun receiveEvent(call: EventCall) = try {
        call.channel.receive()
    } catch (e: DaemonTransport.DaemonException) {
        throw e.withCurrentTrace()
    } catch (e: ClosedReceiveChannelException) {
        throw IOException("$BINARY_NAME call ${call.id} completed before event", e)
    }

    private suspend fun closeEventCall(id: Long) {
        lock.withLock {
            cancelCallLocked(id)
            maybeShutdownLocked()
        }
    }

    private fun nextCallIdLocked(): Long {
        while (calls.containsKey(nextCallId)) nextCallId = if (nextCallId == Long.MAX_VALUE) 1 else nextCallId + 1
        return nextCallId.also { nextCallId = if (it == Long.MAX_VALUE) 1 else it + 1 }
    }

    private fun startReaderLocked(input: ByteReadChannel) {
        readerJob = logScope.launch {
            try {
                while (currentCoroutineContext().isActive) when (val frame = DaemonTransport.readFrame(
                    DaemonIpc.readFrame(input))) {
                    is DaemonTransport.Frame.Reply -> {
                        val reply = lock.withLock {
                            when (val call = calls[frame.id]) {
                                is Call.OneShot -> {
                                    calls.remove(frame.id)
                                    maybeShutdownLocked()
                                    call.reply
                                }
                                else -> {
                                    Timber.w("Unexpected $BINARY_NAME reply for call ${frame.id}")
                                    null
                                }
                            }
                        }
                        reply?.complete(frame.packet)
                    }
                    is DaemonTransport.Frame.Event -> {
                        deliverEventCallFrame(frame.id, frame.packet)
                    }
                    is DaemonTransport.Frame.Error -> {
                        val call = lock.withLock {
                            val call = calls.remove(frame.id)
                            if (call == null) Timber.w("Unexpected $BINARY_NAME error for call ${frame.id}")
                            maybeShutdownLocked()
                            call
                        }
                        when (call) {
                            is Call.OneShot -> call.reply.completeExceptionally(frame.exception)
                            is Call.Event -> call.channel.close(frame.exception)
                            null -> { }
                        }
                    }
                    is DaemonTransport.Frame.NonFatal -> {
                        val traced = frame.exception.withCurrentTrace()
                        Timber.tag(BINARY_NAME).w(traced)
                        SmartSnackbar.make(traced).show()
                    }
                    is DaemonTransport.Frame.Complete -> {
                        val protocolError = lock.withLock {
                            when (val call = calls.remove(frame.id)) {
                                is Call.Event -> {
                                    call.channel.close()
                                    maybeShutdownLocked()
                                    null
                                }
                                is Call.OneShot -> {
                                    val error = IOException("Unexpected $BINARY_NAME complete for one-shot call ${frame.id}")
                                    call.reply.completeExceptionally(error)
                                    maybeShutdownLocked()
                                    error
                                }
                                null -> {
                                    Timber.tag(BINARY_NAME).w("Unexpected $BINARY_NAME complete for unknown call ${frame.id}")
                                    null
                                }
                            }
                        }
                        if (protocolError != null) lock.withLock {
                            closeAndClearStateLocked(cause = protocolError)
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

    private suspend fun deliverEventCallFrame(id: Long, packet: ByteArray): Boolean {
        val call = lock.withLock { calls[id] as? Call.Event }
        if (call == null) {
            Timber.d("Dropping event for unknown $BINARY_NAME call $id")
            return false
        }
        val result = call.channel.trySend(packet)
        if (result.isFailure) {
            result.exceptionOrNull()?.let { call.channel.close(it) }
            lock.withLock {
                if (calls[id] === call) {
                    cancelCallLocked(id)
                    maybeShutdownLocked()
                }
            }
        }
        return true
    }

    private fun completeCallsLocked(e: Throwable) {
        for (call in calls.values) when (call) {
            is Call.OneShot -> call.reply.completeExceptionally(e)
            is Call.Event -> call.channel.close(e)
        }
        calls.clear()
    }

    private suspend fun cancelCallLocked(id: Long) {
        val call = calls.remove(id) ?: return
        if (call is Call.Event) call.channel.close()
        if (output == null) return
        try {
            writePacketLocked(DaemonTransport.call(id, DaemonProtocol.cancel().packet))
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Timber.w(e)
            closeConnectionLocked()
        }
    }

    private suspend fun maybeShutdownLocked() {
        if (output == null || calls.isNotEmpty()) return
        closeConnectionLocked()
    }

    private suspend fun closeAndClearStateLocked(
        cancelReader: Boolean = true,
        cause: Throwable? = null,
    ) {
        if (cause != null) completeCallsLocked(cause)
        closeConnectionLocked(cancelReader)
    }

    private suspend fun writePacketLocked(packet: ByteArray) {
        DaemonIpc.writeFrame(output!!, packet)
    }

    private suspend fun closeConnectionLocked(cancelReader: Boolean = true) = withContext(NonCancellable) {
        val wasConnected = socket != null || input != null || output != null || readerJob != null
        if (wasConnected) Timber.d("Stopping $BINARY_NAME")
        daemonStdioClosing = true
        completeCallsLocked(IOException("$BINARY_NAME connection closed"))
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
        if (wasConnected) Timber.d("Stopped $BINARY_NAME")
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
                            if (!daemonStdioClosing && calls.isNotEmpty() && !daemonStdioEofReported) {
                                daemonStdioEofReported = true
                                Timber.w(DaemonStdioEofException("$BINARY_NAME $stream EOF calls=${calls.size}"))
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
