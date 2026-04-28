package be.mygod.vpnhotspot.net.ipv6

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
import be.mygod.vpnhotspot.root.DaemonIpc
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.RunDaemon
import be.mygod.vpnhotspot.util.Services
import dalvik.system.BaseDexClassLoader
import io.ktor.utils.io.ByteReadChannel
import io.ktor.utils.io.readLineTo
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.launch
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import timber.log.Timber
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlin.random.Random
import kotlin.time.Duration.Companion.seconds

object Ipv6NatController {
    private const val BINARY_NAME = "vpnhotspotd"

    private val lock = Mutex()
    private val activeSessions = mutableSetOf<String>()
    private var socket: LocalSocket? = null
    private var input: InputStream? = null
    private var output: OutputStream? = null
    private var connectionFile: File? = null
    private var stdoutLogChannel: ParcelFileDescriptorReadChannel? = null
    private var stderrLogChannel: ParcelFileDescriptorReadChannel? = null
    private val stdoutLogLine = StringBuilder()
    private val stderrLogLine = StringBuilder()
    private val logScope = CoroutineScope(Dispatchers.Main.immediate + SupervisorJob())
    private var stdoutLogJob: Job? = null
    private var stderrLogJob: Job? = null

    private val rootDir by lazy { File(app.deviceStorage.codeCacheDir, "root").apply { mkdirs() } }

    /**
     * Android 10 bionic supports direct linker execution of uncompressed, page-aligned zip entries:
     * https://android.googlesource.com/platform/bionic/+/android-10.0.0_r7/linker/linker_main.cpp#663
     * Android 10 DexPathList returns zip native-library paths only for stored entries:
     * https://android.googlesource.com/platform/libcore/+/android-10.0.0_r1/dalvik/src/main/java/dalvik/system/DexPathList.java#884
     */
    private val daemonCommand by lazy {
        val path = (app.classLoader as BaseDexClassLoader).findLibrary(BINARY_NAME) ?: error("Daemon JNI missing")
        listOf(if (Process.is64Bit()) "/system/bin/linker64" else "/system/bin/linker", path)
    }

    internal suspend fun startSession(config: Ipv6NatProtocol.SessionConfig) = lock.withLock {
        ensureDaemonLocked()
        try {
            writePacketLocked(Ipv6NatProtocol.startSession(config))
            Ipv6NatProtocol.readPorts(readPacketLocked()).also {
                activeSessions.add(config.sessionId)
            }
        } catch (e: Exception) {
            closeConnectionLocked()
            activeSessions.clear()
            throw e
        }
    }

    internal suspend fun replaceSession(config: Ipv6NatProtocol.SessionConfig) = lock.withLock {
        ensureDaemonLocked()
        try {
            writePacketLocked(Ipv6NatProtocol.replaceSession(config))
            Ipv6NatProtocol.readAck(readPacketLocked())
        } catch (e: Exception) {
            closeConnectionLocked()
            activeSessions.clear()
            throw e
        }
    }

    internal suspend fun removeSession(sessionId: String) = lock.withLock {
        if (output == null) return@withLock
        try {
            writePacketLocked(Ipv6NatProtocol.removeSession(sessionId))
            Ipv6NatProtocol.readAck(readPacketLocked())
        } catch (e: Exception) {
            closeConnectionLocked()
            activeSessions.clear()
            Timber.w(e)
        }
        activeSessions.remove(sessionId)
        if (activeSessions.isEmpty()) {
            try {
                if (output != null) writePacketLocked(Ipv6NatProtocol.shutdown())
            } catch (e: Exception) {
                Timber.w(e)
            }
            closeConnectionLocked()
        }
    }

    private suspend fun ensureDaemonLocked() {
        if (socket != null) return
        val socketName = "be.mygod.vpnhotspot.${Process.myPid()}.${Random.nextLong().toHexString()}"
        val connectionFile = File(rootDir, "$socketName.connection")
        check(connectionFile.createNewFile()) { "Failed to create ${connectionFile.absolutePath}" }
        this.connectionFile = connectionFile
        var stdout: ParcelFileDescriptor? = null
        var stderr: ParcelFileDescriptor? = null
        var stdoutRead: ParcelFileDescriptor? = null
        var stderrRead: ParcelFileDescriptor? = null
        try {
            val stdoutPipe = ParcelFileDescriptor.createPipe()
            val stdoutReadEnd = stdoutPipe[0]
            stdoutRead = stdoutReadEnd
            stdout = stdoutPipe[1]
            val stderrPipe = ParcelFileDescriptor.createPipe()
            val stderrReadEnd = stderrPipe[0]
            stderrRead = stderrReadEnd
            stderr = stderrPipe[1]
            val stdoutChannel = stdoutReadEnd.openReadChannel(Services.mainHandler.looper)
            stdoutLogChannel = stdoutChannel
            stdoutRead = null
            val stderrChannel = stderrReadEnd.openReadChannel(Services.mainHandler.looper)
            stderrLogChannel = stderrChannel
            stderrRead = null
            stdoutLogJob = readLog(stdoutChannel, stdoutLogLine) { Timber.tag(BINARY_NAME).i(it) }
            stderrLogJob = readLog(stderrChannel, stderrLogLine) { Timber.tag(BINARY_NAME).e(it) }
            LocalServerSocket(socketName).use { serverSocket ->
                RootManager.use { server ->
                    try {
                        server.execute(RunDaemon(
                            daemonCommand,
                            socketName,
                            connectionFile.absolutePath,
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
                    input = it.inputStream
                    output = it.outputStream
                }
            }
        } catch (e: Exception) {
            try {
                stdoutRead?.close()
            } catch (_: IOException) { }
            try {
                stderrRead?.close()
            } catch (_: IOException) { }
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

    private fun writePacketLocked(packet: ByteArray) {
        DaemonIpc.writeFrame(output!!, packet)
        output!!.flush()
    }

    private fun readPacketLocked() = DaemonIpc.readFrame(input!!)

    private fun readLog(channel: ByteReadChannel, line: StringBuilder, log: (String) -> Unit) = logScope.launch {
        try {
            while (channel.readLineTo(line) >= 0) {
                log(line.toString())
                line.clear()
            }
        } catch (e: ErrnoException) {
            if (e.errno != OsConstants.EBADF) Timber.w(e)
        }
    }

    private suspend fun closeConnectionLocked() = withContext(NonCancellable) {
        try {
            input?.close()
        } catch (_: IOException) { }
        try {
            output?.close()
        } catch (_: IOException) { }
        try {
            socket?.close()
        } catch (_: IOException) { }
        withContext(Dispatchers.Main.immediate) {
            stdoutLogJob?.cancelAndJoin()
            stderrLogJob?.cancelAndJoin()
            stdoutLogJob = null
            stderrLogJob = null
            stdoutLogChannel?.let {
                try {
                    it.drain()
                    it.drainLines(stdoutLogLine, flushPartial = true) { line ->
                        Timber.tag(BINARY_NAME).i(line)
                    }
                } catch (e: ErrnoException) {
                    if (e.errno != OsConstants.EBADF) Timber.w(e)
                } finally {
                    it.cancel(null)
                }
            }
            stderrLogChannel?.let {
                try {
                    it.drain()
                    it.drainLines(stderrLogLine, flushPartial = true) { line ->
                        Timber.tag(BINARY_NAME).e(line)
                    }
                } catch (e: ErrnoException) {
                    if (e.errno != OsConstants.EBADF) Timber.w(e)
                } finally {
                    it.cancel(null)
                }
            }
        }
        input = null
        output = null
        socket = null
        stdoutLogChannel = null
        stderrLogChannel = null
        connectionFile?.delete()
        connectionFile = null
    }
}
