package be.mygod.vpnhotspot.net.ipv6

import android.net.LocalServerSocket
import android.net.LocalSocket
import android.os.Build
import android.os.Process
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import android.system.StructPollfd
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.root.DaemonIpc
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.RunDaemon
import kotlinx.coroutines.CoroutineExceptionHandler
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import timber.log.Timber
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
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

    private fun acceptLocked(serverSocket: LocalServerSocket): LocalSocket {
        val pollFd = StructPollfd().apply {
            fd = serverSocket.fileDescriptor
            events = OsConstants.POLLIN.toShort()
        }
        val ready = try {
            Os.poll(arrayOf(pollFd), DaemonIpc.STARTUP_TIMEOUT_MILLIS.toInt())
        } catch (e: ErrnoException) {
            throw e.rethrowAsIOException()
        }
        if (ready == 0) throw IOException("Timed out waiting for $BINARY_NAME to connect")
        if ((pollFd.revents.toInt() and OsConstants.POLLIN) == 0) {
            throw IOException("Unexpected $BINARY_NAME listener event ${pollFd.revents}")
        }
        return serverSocket.accept()
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
