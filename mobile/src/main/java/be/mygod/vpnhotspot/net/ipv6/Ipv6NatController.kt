package be.mygod.vpnhotspot.net.ipv6

import android.net.LocalServerSocket
import android.net.LocalSocket
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.MessageQueue
import android.os.Process
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.root.DaemonIpc
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.RunDaemon
import kotlinx.coroutines.CoroutineExceptionHandler
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import timber.log.Timber
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlin.random.Random

object Ipv6NatController : CoroutineScope {
    private const val BINARY_NAME = "vpnhotspotd"

    override val coroutineContext = Dispatchers.IO + SupervisorJob() +
            CoroutineExceptionHandler { _, t -> Timber.w(t) }
    private val lock = Mutex()
    private val activeSessions = mutableSetOf<String>()
    private var socket: LocalSocket? = null
    private var input: InputStream? = null
    private var output: OutputStream? = null
    private var connectionFile: File? = null

    private val rootDir by lazy { File(app.deviceStorage.codeCacheDir, "root").apply { mkdirs() } }
    private val daemonPath by lazy { File(rootDir, BINARY_NAME) }
    private val logPath by lazy { File(rootDir, "$BINARY_NAME.log") }
    private val acceptHandler by lazy { Handler(Looper.getMainLooper()) }

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
        daemonPath.delete()
        extractBinaryLocked()
        val socketName = newSocketName()
        val connectionFile = File(rootDir, "$socketName.connection")
        check(connectionFile.createNewFile()) { "Failed to create ${connectionFile.absolutePath}" }
        this.connectionFile = connectionFile
        try {
            LocalServerSocket(socketName).use { serverSocket ->
                RootManager.use { server ->
                    server.execute(RunDaemon(
                        daemonPath.absolutePath,
                        socketName,
                        connectionFile.absolutePath,
                        logPath.absolutePath,
                    ))
                }
                acceptLocked(serverSocket).also {
                    socket = it
                    input = it.inputStream
                    output = it.outputStream
                }
            }
        } catch (e: Exception) {
            closeConnectionLocked()
            throw e
        }
    }

    private fun newSocketName() = "be.mygod.vpnhotspot.${Process.myPid()}.${Random.nextLong().toHexString()}"

    private suspend fun acceptLocked(serverSocket: LocalServerSocket): LocalSocket {
        try {
            val flags = Os.fcntlInt(serverSocket.fileDescriptor, OsConstants.F_GETFL, 0)
            Os.fcntlInt(serverSocket.fileDescriptor, OsConstants.F_SETFL, flags or OsConstants.O_NONBLOCK)
        } catch (e: ErrnoException) {
            throw e.rethrowAsIOException()
        }
        return suspendCancellableCoroutine { continuation ->
            val completed = AtomicBoolean(false)
            val queue = acceptHandler.looper.queue
            val descriptor = serverSocket.fileDescriptor
            lateinit var timeout: Runnable
            lateinit var listener: MessageQueue.OnFileDescriptorEventListener
            fun finish(block: () -> Unit): Int {
                if (completed.compareAndSet(false, true)) {
                    acceptHandler.removeCallbacks(timeout)
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
                if (completed.compareAndSet(false, true)) acceptHandler.post {
                    acceptHandler.removeCallbacks(timeout)
                    queue.removeOnFileDescriptorEventListener(descriptor)
                }
            }
            acceptHandler.post {
                if (completed.get()) return@post
                queue.addOnFileDescriptorEventListener(descriptor,
                    MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT or
                            MessageQueue.OnFileDescriptorEventListener.EVENT_ERROR, listener)
                acceptHandler.postDelayed(timeout, DaemonIpc.STARTUP_TIMEOUT_MILLIS)
            }
        }
    }

    private fun extractBinaryLocked() {
        val abi = Build.SUPPORTED_ABIS.firstOrNull {
            try {
                app.assets.open("daemon/$it/$BINARY_NAME").close()
                true
            } catch (_: IOException) {
                false
            }
        } ?: error("No supported IPv6 NAT daemon asset found")
        app.assets.open("daemon/$abi/$BINARY_NAME").use { input ->
            daemonPath.outputStream().use { output -> input.copyTo(output) }
        }
        check(daemonPath.setExecutable(true, true)) { "Failed to chmod ${daemonPath.absolutePath}" }
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
        input = null
        output = null
        socket = null
        connectionFile?.delete()
        connectionFile = null
    }
}
