package be.mygod.vpnhotspot.net.ipv6

import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.os.Build
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.root.KillDaemon
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.RunDaemon
import kotlinx.coroutines.CoroutineExceptionHandler
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import timber.log.Timber
import java.io.EOFException
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream

@RequiresApi(29)
object Ipv6NatController : CoroutineScope {
    private const val BINARY_NAME = "vpnhotspotd"
    private const val MAX_PACKET_SIZE = 65535

    override val coroutineContext = Dispatchers.IO + SupervisorJob() +
            CoroutineExceptionHandler { _, t -> Timber.w(t) }
    private val lock = Mutex()
    private val activeSessions = mutableSetOf<String>()
    private var socket: LocalSocket? = null
    private var input: InputStream? = null
    private var output: OutputStream? = null

    private val rootDir by lazy { File(app.deviceStorage.codeCacheDir, "root").apply { mkdirs() } }
    private val daemonPath by lazy { File(rootDir, BINARY_NAME) }
    private val socketPath by lazy { File(rootDir, "$BINARY_NAME.sock") }

    internal suspend fun startSession(config: Ipv6NatProtocol.SessionConfig): Ipv6NatProtocol.SessionPorts =
        lock.withLock {
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

    internal suspend fun replaceSession(config: Ipv6NatProtocol.SessionConfig): Unit = lock.withLock {
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

    internal suspend fun removeSession(sessionId: String): Unit = lock.withLock {
        val output = output ?: return
        try {
            output.write(Ipv6NatProtocol.removeSession(sessionId))
            output.flush()
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
        if (socketPath.exists()) try {
            LocalSocket(LocalSocket.SOCKET_SEQPACKET).use { socket ->
                socket.connect(LocalSocketAddress(socketPath.absolutePath, LocalSocketAddress.Namespace.FILESYSTEM))
                socket.outputStream.write(Ipv6NatProtocol.shutdown())
                socket.outputStream.flush()
            }
        } catch (_: IOException) { }
            daemonPath.delete()
            socketPath.delete()
            extractBinaryLocked()
        try {
            RootManager.use { server ->
                server.execute(KillDaemon(daemonPath.absolutePath))
                server.execute(RunDaemon(daemonPath.absolutePath, socketPath.absolutePath))
            }
            connectLocked()
        } catch (e: Exception) {
            RootManager.use { it.execute(KillDaemon(daemonPath.absolutePath)) }
            closeConnectionLocked()
            throw e
        }
    }

    private suspend fun connectLocked() {
        repeat(100) {
            try {
                val socket = LocalSocket(LocalSocket.SOCKET_SEQPACKET)
                socket.connect(LocalSocketAddress(socketPath.absolutePath, LocalSocketAddress.Namespace.FILESYSTEM))
                this.socket = socket
                input = socket.inputStream
                output = socket.outputStream
                return
            } catch (e: IOException) {
                delay(50)
            }
        }
        throw IOException("Failed to connect to $socketPath")
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

    private suspend fun shutdownLocked() {
        try {
            output?.let {
                it.write(Ipv6NatProtocol.shutdown())
                it.flush()
            }
        } catch (e: Exception) {
            Timber.w(e)
        }
        closeConnectionLocked()
        activeSessions.clear()
    }

    private fun writePacketLocked(packet: ByteArray) {
        output!!.write(packet)
        output!!.flush()
    }

    private fun readPacketLocked(): ByteArray {
        val buffer = ByteArray(MAX_PACKET_SIZE)
        val length = input!!.read(buffer)
        if (length < 0) throw EOFException("IPv6 NAT daemon disconnected")
        return buffer.copyOf(length)
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
        input = null
        output = null
        socket = null
    }
}
