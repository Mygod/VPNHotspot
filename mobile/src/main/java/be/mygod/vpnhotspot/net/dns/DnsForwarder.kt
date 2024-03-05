package be.mygod.vpnhotspot.net.dns

import be.mygod.vpnhotspot.net.monitor.FallbackUpstreamMonitor
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import io.ktor.network.selector.SelectorManager
import io.ktor.network.sockets.BoundDatagramSocket
import io.ktor.network.sockets.Datagram
import io.ktor.network.sockets.InetSocketAddress
import io.ktor.network.sockets.ServerSocket
import io.ktor.network.sockets.Socket
import io.ktor.network.sockets.aSocket
import io.ktor.network.sockets.openReadChannel
import io.ktor.network.sockets.openWriteChannel
import io.ktor.network.sockets.tcpNoDelay
import io.ktor.network.sockets.toJavaAddress
import io.ktor.util.network.port
import io.ktor.utils.io.core.ByteReadPacket
import io.ktor.utils.io.core.readBytes
import io.ktor.utils.io.writeFully
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineExceptionHandler
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.ClosedReceiveChannelException
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.newSingleThreadContext
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import org.xbill.DNS.Message
import org.xbill.DNS.Rcode
import timber.log.Timber
import java.io.IOException
import java.nio.channels.ClosedChannelException

/**
 * Downstream user should make sure to also register for at least one UpstreamMonitors, which is also used by this forwarder.
 */
class DnsForwarder : CoroutineScope {
    companion object {
        private var instance: DnsForwarder? = null

        private val clients = mutableSetOf<Any>()
        fun registerClient(client: Any, useLocalnet: Boolean) = synchronized(clients) {
            (instance ?: DnsForwarder().apply {
                try {
                    start(useLocalnet)
                } catch (e: Exception) {
                    stop()
                    throw e
                }
                instance = this
            }).also { clients.add(client) }
        }
        fun unregisterClient(client: Any) = synchronized(clients) {
            clients.remove(client).also {
                if (it && clients.isEmpty()) {
                    instance?.stop()
                    instance = null
                }
            }
        }

        private val localhostAnyPort = InetSocketAddress("127.0.0.1", 0)
    }

    override val coroutineContext = Dispatchers.Default + SupervisorJob() +
            CoroutineExceptionHandler { _, t -> Timber.w(t) }
    private var tcp: ServerSocket? = null
    private var udp: BoundDatagramSocket? = null
    val tcpPort get() = tcp!!.localAddress.toJavaAddress().port
    val udpPort get() = udp!!.localAddress.toJavaAddress().port

    private fun start(useLocalnet: Boolean) {
        val selectorManager = VpnProtectedSelectorManager(SelectorManager(
            coroutineContext + newSingleThreadContext("DnsForwarder")))
        val t = aSocket(selectorManager).tcp().tcpNoDelay().bind(if (useLocalnet) localhostAnyPort else null)
        tcp = t
        val u = aSocket(selectorManager).udp().bind(if (useLocalnet) localhostAnyPort else null)
        udp = u
        launch {
            while (true) {
                val socket = try {
                    t.accept()
                } catch (e: ClosedChannelException) {
                    break
                }
                handleAsync(socket)
            }
        }
        launch {
            while (true) {
                val packet = try {
                    u.receive()
                } catch (e: ClosedChannelException) {
                    break
                }
                handleAsync(packet)
            }
        }
    }
    private fun stop() {
        cancel("All clients are gone")
        tcp?.close()
        udp?.close()
    }

    private fun handleAsync(socket: Socket) = launch connection@ {
//        Timber.d("Incoming tcp:${socket.remoteAddress.toJavaAddress()}")
        socket.use { socket ->
            coroutineScope {
                try {
                    val reader = socket.openReadChannel()
                    val writer = socket.openWriteChannel()
                    val writerMutex = Mutex()
                    while (!reader.isClosedForRead) {
                        val query = ByteArray(reader.readShort().toUShort().toInt())
                        reader.readFully(query, 0, query.size)
                        launch {
                            val response = resolve(query) {
                                "Packet from tcp:${socket.remoteAddress.toJavaAddress()}"
                            } ?: return@launch
                            writerMutex.withLock {
                                try {
                                    writer.writeShort(response.size.toShort())
                                    writer.writeFully(response)
                                    writer.flush()
                                } catch (e: IOException) {
                                    Timber.d(e, "Cannot write to tcp:${socket.remoteAddress.toJavaAddress()}")
                                    cancel("Write fail")
                                }
                            }
                        }
                    }
                } catch (e: IOException) {
                    Timber.d(e, "Failed to handle connection to tcp:${socket.remoteAddress.toJavaAddress()}")
                    cancel("Main loop error")
                } catch (e: ClosedReceiveChannelException) {
                    Timber.d(e, "EOF from tcp:${socket.remoteAddress.toJavaAddress()}")
                    cancel("EOF from read")
                }
            }
        }
    }

    private fun handleAsync(datagram: Datagram) = launch {
//        Timber.d("Incoming udp:${datagram.address.toJavaAddress()}")
        try {
            val query = datagram.packet.readBytes()
            val response = resolve(query) { "Packet from udp:${datagram.address.toJavaAddress()}" } ?: return@launch
            udp!!.send(Datagram(ByteReadPacket(response), datagram.address))
        } catch (e: IOException) {
            Timber.d(e, "Failed to handle connection from udp:${datagram.address.toJavaAddress()}")
        }
    }

    private suspend fun resolve(query: ByteArray, source: () -> String) = try {
        DnsResolverCompat.resolveRaw(UpstreamMonitor.currentNetwork ?: FallbackUpstreamMonitor.currentNetwork ?:
            throw IOException("no upstream available"), query)
    } catch (e: Exception) {
        when (e) {
            is CancellationException -> { }
            is IOException, is UnsupportedOperationException -> Timber.d(e, source())
            else -> Timber.w(e, source())
        }
        try {
            DnsResolverCompat.prepareDnsResponse(Message(query)).apply {
                header.rcode = if (e is UnsupportedOperationException) Rcode.NOTIMP else Rcode.SERVFAIL
            }.toWire()
        } catch (e: IOException) {
            Timber.d("Malformed ${source()}", e)
            null    // return empty if cannot parse packet
        }
    }
}
