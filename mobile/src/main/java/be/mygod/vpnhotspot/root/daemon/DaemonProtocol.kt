package be.mygod.vpnhotspot.root.daemon

import android.net.LinkProperties
import android.net.Network
import be.mygod.vpnhotspot.util.allInterfaceNames
import be.mygod.vpnhotspot.util.allRoutes
import kotlinx.io.Buffer
import kotlinx.io.Sink
import kotlinx.io.Source
import kotlinx.io.readByteArray
import kotlinx.io.readString
import java.io.IOException
import java.net.Inet6Address

internal object DaemonProtocol {
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
        val routes: List<Route>,
    ) {
        companion object {
            fun from(network: Network?, properties: LinkProperties?) = if (network == null || properties == null) {
                null
            } else Upstream(network.networkHandle,
                properties.interfaceName ?: properties.allInterfaceNames.firstOrNull().orEmpty(),
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
        val downstream: String,
        val dnsBindAddress: String,
        val replyMark: Int,
        val primary: Upstream?,
        val fallback: Upstream?,
        val ipv6Nat: Ipv6NatConfig?,
    )

    data class Ipv6NatConfig(
        val router: String,
        val gateway: String,
        val prefixLength: Int,
        val mtu: Int,
        val suppressedPrefixes: List<Route>,
        val cleanupPrefixes: List<Route>,
    )

    data class SessionPorts(
        val dnsTcp: Int,
        val dnsUdp: Int,
        val ipv6Nat: Ipv6NatPorts?,
    )

    data class Ipv6NatPorts(
        val tcp: Int,
        val udp: Int,
    )

    enum class RemoveMode(val protocolValue: Byte) {
        PreserveCleanup(0),
        WithdrawCleanup(1),
    }

    fun startSession(config: SessionConfig) = writePacket(CMD_START_SESSION) { writeSession(config) }
    fun replaceSession(config: SessionConfig) = writePacket(CMD_REPLACE_SESSION) { writeSession(config) }
    fun removeSession(sessionId: String, mode: RemoveMode) = writePacket(CMD_REMOVE_SESSION) {
        writeUtf(sessionId)
        writeByte(mode.protocolValue)
    }
    fun shutdown(mode: RemoveMode) = writePacket(CMD_SHUTDOWN) { writeByte(mode.protocolValue) }

    fun readPorts(packet: ByteArray): SessionPorts {
        val input = Buffer().apply { write(packet) }
        readStatus(input)
        return SessionPorts(
            input.readUnsignedShortValue(),
            input.readUnsignedShortValue(),
            if (input.readByte() != 0.toByte()) {
                Ipv6NatPorts(input.readUnsignedShortValue(), input.readUnsignedShortValue())
            } else null,
        )
    }

    fun readAck(packet: ByteArray) {
        readStatus(Buffer().apply { write(packet) })
    }

    private fun writePacket(command: Int, block: Sink.() -> Unit): ByteArray {
        val output = Buffer()
        output.writeInt(command)
        output.block()
        return output.readByteArray()
    }

    private fun Sink.writeSession(config: SessionConfig) {
        writeUtf(config.sessionId)
        writeUtf(config.downstream)
        writeUtf(config.dnsBindAddress)
        writeInt(config.replyMark)
        writeUpstream(config.primary)
        writeUpstream(config.fallback)
        val ipv6Nat = config.ipv6Nat
        writeByte((if (ipv6Nat != null) 1 else 0).toByte())
        if (ipv6Nat == null) return
        writeUtf(ipv6Nat.router)
        writeUtf(ipv6Nat.gateway)
        writeInt(ipv6Nat.prefixLength)
        writeInt(ipv6Nat.mtu)
        writeInt(ipv6Nat.suppressedPrefixes.size)
        for (prefix in ipv6Nat.suppressedPrefixes) {
            writeUtf(prefix.address)
            writeInt(prefix.prefixLength)
        }
        writeInt(ipv6Nat.cleanupPrefixes.size)
        for (prefix in ipv6Nat.cleanupPrefixes) {
            writeUtf(prefix.address)
            writeInt(prefix.prefixLength)
        }
    }

    private fun Sink.writeUpstream(upstream: Upstream?) {
        writeByte((if (upstream != null) 1 else 0).toByte())
        if (upstream == null) return
        writeLong(upstream.networkHandle)
        writeUtf(upstream.interfaceName)
        writeInt(upstream.routes.size)
        for (route in upstream.routes) {
            writeUtf(route.address)
            writeInt(route.prefixLength)
        }
    }

    private fun readStatus(input: Source) {
        when (val status = input.readByte().toInt() and 0xFF) {
            STATUS_OK -> { }
            STATUS_ERROR -> throw IOException(input.readUtf())
            else -> throw IOException("Unknown daemon status $status")
        }
    }

    private fun Sink.writeUtf(value: String) {
        val bytes = value.encodeToByteArray()
        require(bytes.size <= UShort.MAX_VALUE.toInt()) { "String too long" }
        writeShort(bytes.size.toShort())
        write(bytes)
    }

    private fun Source.readUnsignedShortValue() = readShort().toInt() and 0xFFFF

    private fun Source.readUtf() = readString(readUnsignedShortValue().toLong())
}
