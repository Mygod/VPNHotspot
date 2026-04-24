package be.mygod.vpnhotspot.net.ipv6

import android.net.LinkProperties
import android.net.Network
import be.mygod.vpnhotspot.util.allInterfaceNames
import be.mygod.vpnhotspot.util.allRoutes
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException
import java.net.Inet6Address
import java.net.InetAddress

internal object Ipv6NatProtocol {
    const val STATUS_OK = 0
    const val STATUS_ERROR = 1

    const val CMD_START_SESSION = 1
    const val CMD_REPLACE_SESSION = 2
    const val CMD_REMOVE_SESSION = 3
    const val CMD_SHUTDOWN = 4

    data class Route(
        val address: String,
        val prefixLength: Int,
    )

    data class Upstream(
        val networkHandle: Long,
        val interfaceName: String,
        val dnsServers: List<String>,
        val routes: List<Route>,
    ) {
        companion object {
            fun from(network: Network?, properties: LinkProperties?) = if (network == null || properties == null) {
                null
            } else Upstream(network.networkHandle,
                properties.interfaceName ?: properties.allInterfaceNames.firstOrNull().orEmpty(),
                properties.dnsServers.mapNotNull(InetAddress::getHostAddress),
                properties.allRoutes.mapNotNull { route ->
                    val destination = route.destination
                    val address = destination.address
                    if (address !is Inet6Address) null else {
                        val hostAddress = address.hostAddress ?: return@mapNotNull null
                        Route(hostAddress, destination.prefixLength)
                    }
                })
        }
    }

    data class SessionConfig(
        val sessionId: String,
        val generationId: Int,
        val downstream: String,
        val router: String,
        val gateway: String,
        val prefixLength: Int,
        val replyMark: Int,
        val dnsBindAddress: String,
        val mtu: Int,
        val deprecatedPrefixes: List<Route>,
        val primary: Upstream?,
        val fallback: Upstream?,
    )

    data class SessionPorts(
        val tcp: Int,
        val udp: Int,
        val dnsTcp: Int,
        val dnsUdp: Int,
    )

    fun startSession(config: SessionConfig) = writePacket(CMD_START_SESSION) { writeSession(config) }
    fun replaceSession(config: SessionConfig) = writePacket(CMD_REPLACE_SESSION) { writeSession(config) }
    fun removeSession(sessionId: String) = writePacket(CMD_REMOVE_SESSION) { writeUTF(sessionId) }
    fun shutdown() = writePacket(CMD_SHUTDOWN) { }

    fun readPorts(packet: ByteArray): SessionPorts {
        val input = DataInputStream(ByteArrayInputStream(packet))
        readStatus(input)
        return SessionPorts(input.readUnsignedShort(), input.readUnsignedShort(),
            input.readUnsignedShort(), input.readUnsignedShort())
    }

    fun readAck(packet: ByteArray) {
        readStatus(DataInputStream(ByteArrayInputStream(packet)))
    }

    private fun writePacket(command: Int, block: DataOutputStream.() -> Unit): ByteArray {
        val bytes = ByteArrayOutputStream()
        DataOutputStream(bytes).use { output ->
            output.writeInt(command)
            output.block()
        }
        return bytes.toByteArray()
    }

    private fun DataOutputStream.writeSession(config: SessionConfig) {
        writeUTF(config.sessionId)
        writeInt(config.generationId)
        writeUTF(config.downstream)
        writeUTF(config.router)
        writeUTF(config.gateway)
        writeInt(config.prefixLength)
        writeInt(config.replyMark)
        writeUTF(config.dnsBindAddress)
        writeInt(config.mtu)
        writeInt(config.deprecatedPrefixes.size)
        for (prefix in config.deprecatedPrefixes) {
            writeUTF(prefix.address)
            writeInt(prefix.prefixLength)
        }
        writeUpstream(config.primary)
        writeUpstream(config.fallback)
    }

    private fun DataOutputStream.writeUpstream(upstream: Upstream?) {
        writeBoolean(upstream != null)
        if (upstream == null) return
        writeLong(upstream.networkHandle)
        writeUTF(upstream.interfaceName)
        writeInt(upstream.dnsServers.size)
        for (server in upstream.dnsServers) writeUTF(server)
        writeInt(upstream.routes.size)
        for (route in upstream.routes) {
            writeUTF(route.address)
            writeInt(route.prefixLength)
        }
    }

    private fun readStatus(input: DataInputStream) {
        when (val status = input.readUnsignedByte()) {
            STATUS_OK -> { }
            STATUS_ERROR -> throw IOException(input.readUTF())
            else -> throw IOException("Unknown daemon status $status")
        }
    }
}
