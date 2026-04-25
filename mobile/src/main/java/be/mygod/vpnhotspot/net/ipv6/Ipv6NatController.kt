package be.mygod.vpnhotspot.net.ipv6

import android.net.LocalServerSocket
import android.net.LocalSocket
import android.os.MessageQueue
import android.os.ParcelFileDescriptor
import android.os.Process
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.root.DaemonIpc
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.RunDaemon
import be.mygod.vpnhotspot.util.Services
import dalvik.system.BaseDexClassLoader
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import timber.log.Timber
import java.io.File
import java.io.FileDescriptor
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlin.random.Random

object Ipv6NatController {
    private const val BINARY_NAME = "vpnhotspotd"

    private val lock = Mutex()
    private val activeSessions = mutableSetOf<String>()
    private var socket: LocalSocket? = null
    private var input: InputStream? = null
    private var output: OutputStream? = null
    private var connectionFile: File? = null
    private var stdoutLogInput: ParcelFileDescriptor? = null
    private var stderrLogInput: ParcelFileDescriptor? = null
    private val stdoutLogLine = StringBuilder()
    private val stderrLogLine = StringBuilder()

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
        if (activeSessions.isEmpty()) shutdownLocked()
    }

    private suspend fun ensureDaemonLocked() {
        if (socket != null) return
        val socketName = newSocketName()
        val connectionFile = File(rootDir, "$socketName.connection")
        check(connectionFile.createNewFile()) { "Failed to create ${connectionFile.absolutePath}" }
        this.connectionFile = connectionFile
        var stdout: ParcelFileDescriptor? = null
        var stderr: ParcelFileDescriptor? = null
        try {
            val stdoutRead = ParcelFileDescriptor.createPipe().let {
                stdout = it[1]
                it[0].also { read -> stdoutLogInput = read }
            }
            val stderrRead = ParcelFileDescriptor.createPipe().let {
                stderr = it[1]
                it[0].also { read -> stderrLogInput = read }
            }
            readLog(stdoutRead.fileDescriptor, stdoutLogLine) { Timber.tag(BINARY_NAME).i(it) }
            readLog(stderrRead.fileDescriptor, stderrLogLine) { Timber.tag(BINARY_NAME).e(it) }
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
                stdout?.close()
            } catch (_: IOException) { }
            try {
                stderr?.close()
            } catch (_: IOException) { }
            closeConnectionLocked()
            throw e
        }
    }

    private fun newSocketName() = "be.mygod.vpnhotspot.${Process.myPid()}.${Random.nextLong().toHexString()}"

    private fun setNonblocking(descriptor: FileDescriptor) {
        try {
            val flags = Os.fcntlInt(descriptor, OsConstants.F_GETFL, 0)
            Os.fcntlInt(descriptor, OsConstants.F_SETFL, flags or OsConstants.O_NONBLOCK)
        } catch (e: ErrnoException) {
            throw e.rethrowAsIOException()
        }
    }

    private suspend fun acceptLocked(serverSocket: LocalServerSocket): LocalSocket {
        setNonblocking(serverSocket.fileDescriptor)
        return suspendCancellableCoroutine { continuation ->
            val completed = AtomicBoolean(false)
            val queue = Services.mainHandler.looper.queue
            val descriptor = serverSocket.fileDescriptor
            lateinit var timeout: Runnable
            lateinit var listener: MessageQueue.OnFileDescriptorEventListener
            fun finish(block: () -> Unit): Int {
                if (completed.compareAndSet(false, true)) {
                    Services.mainHandler.removeCallbacks(timeout)
                    queue.removeOnFileDescriptorEventListener(descriptor)
                    block()
                }
                return 0
            }
            timeout = Runnable {
                finish { continuation.resumeWithException(IOException("Timed out waiting for $BINARY_NAME to connect")) }
            }
            listener = MessageQueue.OnFileDescriptorEventListener { _, events ->
                if (completed.get()) {
                    0
                } else if ((events and MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT) == 0) {
                    finish {
                        continuation.resumeWithException(IOException("Unexpected $BINARY_NAME listener event $events"))
                    }
                } else try {
                    val accepted = serverSocket.accept()
                    finish { continuation.resume(accepted) }
                } catch (e: IOException) {
                    if ((e.cause as? ErrnoException)?.errno == OsConstants.EAGAIN) {
                        MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT or
                                MessageQueue.OnFileDescriptorEventListener.EVENT_ERROR
                    } else finish { continuation.resumeWithException(e) }
                }
            }
            continuation.invokeOnCancellation {
                if (completed.compareAndSet(false, true)) Services.mainHandler.post {
                    Services.mainHandler.removeCallbacks(timeout)
                    queue.removeOnFileDescriptorEventListener(descriptor)
                }
            }
            Services.mainHandler.post {
                if (completed.get()) return@post
                queue.addOnFileDescriptorEventListener(descriptor,
                    MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT or
                            MessageQueue.OnFileDescriptorEventListener.EVENT_ERROR, listener)
                Services.mainHandler.postDelayed(timeout, DaemonIpc.STARTUP_TIMEOUT_MILLIS)
            }
        }
    }

    private fun shutdownLocked() {
        try {
            if (output != null) writePacketLocked(Ipv6NatProtocol.shutdown())
        } catch (e: Exception) {
            Timber.w(e)
        }
        closeConnectionLocked()
        activeSessions.clear()
    }

    private fun writePacketLocked(packet: ByteArray) {
        DaemonIpc.writeFrame(output!!, packet)
        output!!.flush()
    }

    private fun readPacketLocked() = DaemonIpc.readFrame(input!!)

    private fun readLog(descriptor: FileDescriptor, line: StringBuilder, log: (String) -> Unit) {
        setNonblocking(descriptor)
        val buffer = ByteArray(4096)
        Services.mainHandler.looper.queue.addOnFileDescriptorEventListener(descriptor,
            MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT or
                    MessageQueue.OnFileDescriptorEventListener.EVENT_ERROR) { _, events ->
            if ((events and MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT) == 0) {
                flushLog(line, log)
                0
            } else try {
                while (true) {
                    val count = Os.read(descriptor, buffer, 0, buffer.size)
                    if (count == 0) {
                        flushLog(line, log)
                        break
                    }
                    val text = buffer.decodeToString(0, count)
                    var start = 0
                    while (true) {
                        val end = text.indexOf('\n', start)
                        if (end < 0) {
                            line.append(text, start, text.length)
                            break
                        }
                        line.append(text, start, end)
                        flushLog(line, log)
                        start = end + 1
                    }
                }
                0
            } catch (e: ErrnoException) {
                if (e.errno == OsConstants.EAGAIN) {
                    MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT or
                            MessageQueue.OnFileDescriptorEventListener.EVENT_ERROR
                } else {
                    Timber.d(e)
                    0
                }
            }
        }
    }

    private fun flushLog(line: StringBuilder, log: (String) -> Unit) {
        if (line.isNotEmpty()) {
            if (line.last() == '\r') line.setLength(line.length - 1)
            if (line.isNotEmpty()) log(line.toString())
            line.clear()
        }
    }

    private fun closeConnectionLocked() {
        try {
            input?.close()
        } catch (_: IOException) { }
        try {
            output?.close()
        } catch (_: IOException) { }
        try {
            socket?.close()
        } catch (_: IOException) { }
        stdoutLogInput?.fileDescriptor?.let {
            Services.mainHandler.looper.queue.removeOnFileDescriptorEventListener(it)
        }
        stderrLogInput?.fileDescriptor?.let {
            Services.mainHandler.looper.queue.removeOnFileDescriptorEventListener(it)
        }
        flushLog(stdoutLogLine) { Timber.tag(BINARY_NAME).i(it) }
        flushLog(stderrLogLine) { Timber.tag(BINARY_NAME).e(it) }
        try {
            stdoutLogInput?.close()
        } catch (_: IOException) { }
        try {
            stderrLogInput?.close()
        } catch (_: IOException) { }
        input = null
        output = null
        socket = null
        stdoutLogInput = null
        stderrLogInput = null
        connectionFile?.delete()
        connectionFile = null
    }
}
