package be.mygod.vpnhotspot.root.daemon

import android.net.IpPrefix
import android.net.Network
import kotlinx.io.Buffer
import kotlinx.io.Sink
import kotlinx.io.Source
import kotlinx.io.readByteArray
import kotlinx.io.readString
import java.io.IOException
import java.net.Inet4Address
import java.net.Inet6Address

object DaemonProtocol {
    const val STATUS_OK = 0
    const val STATUS_ERROR = 1

    class StatusException(message: String) : IOException(message)

    const val CMD_START_SESSION = 1
    const val CMD_REPLACE_SESSION = 2
    const val CMD_REMOVE_SESSION = 3
    const val CMD_SHUTDOWN = 4

    data class SessionConfig(
        val downstream: String,
        val dnsBindAddress: Inet4Address,
        val replyMark: Int,
        val primaryNetwork: Network?,
        val primaryRoutes: List<IpPrefix>,
        val fallbackNetwork: Network?,
        val ipv6Nat: Ipv6NatConfig?,
    )

    data class Ipv6NatConfig(
        val gateway: Inet6Address,
        val prefixLength: Int,
        val mtu: Int,
        val suppressedPrefixes: List<IpPrefix>,
        val cleanupPrefixes: List<IpPrefix>,
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
    fun removeSession(downstream: String, mode: RemoveMode) = writePacket(CMD_REMOVE_SESSION) {
        writeUtf(downstream)
        writeByte(mode.protocolValue)
    }
    fun shutdown(mode: RemoveMode) = writePacket(CMD_SHUTDOWN) { writeByte(mode.protocolValue) }

    fun readPorts(packet: ByteArray): SessionPorts {
        val input = Buffer().apply { write(packet) }
        readStatus(input)
        return SessionPorts(
            input.readShort().toUShort().toInt(),
            input.readShort().toUShort().toInt(),
            if (input.readByte() != 0.toByte()) {
                Ipv6NatPorts(input.readShort().toUShort().toInt(), input.readShort().toUShort().toInt())
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
        writeUtf(config.downstream)
        writeInet4Address(config.dnsBindAddress)
        writeInt(config.replyMark)
        writeNetwork(config.primaryNetwork)
        writeInt(config.primaryRoutes.size)
        for (route in config.primaryRoutes) writeIpv6Prefix(route)
        writeNetwork(config.fallbackNetwork)
        val ipv6Nat = config.ipv6Nat
        writeByte((if (ipv6Nat != null) 1 else 0).toByte())
        if (ipv6Nat == null) return
        writeInet6Address(ipv6Nat.gateway)
        writeInt(ipv6Nat.prefixLength)
        writeInt(ipv6Nat.mtu)
        writeInt(ipv6Nat.suppressedPrefixes.size)
        for (prefix in ipv6Nat.suppressedPrefixes) writeIpv6Prefix(prefix)
        writeInt(ipv6Nat.cleanupPrefixes.size)
        for (prefix in ipv6Nat.cleanupPrefixes) writeIpv6Prefix(prefix)
    }

    private fun Sink.writeNetwork(network: Network?) = writeLong(network?.networkHandle ?: 0L)

    private fun readStatus(input: Source) {
        when (val status = input.readByte().toInt() and 0xFF) {
            STATUS_OK -> { }
            STATUS_ERROR -> throw StatusException(input.readUtf())
            else -> throw IOException("Unknown daemon status $status")
        }
    }

    private fun Sink.writeUtf(value: String) {
        val bytes = value.encodeToByteArray()
        writeInt(bytes.size)
        write(bytes)
    }

    private fun Sink.writeInet4Address(address: Inet4Address) = write(address.address)

    private fun Sink.writeInet6Address(address: Inet6Address) = write(address.address)

    private fun Sink.writeIpv6Prefix(prefix: IpPrefix) {
        val address = prefix.address
        require(address is Inet6Address) { "IPv6 prefix expected: $prefix" }
        writeInet6Address(address)
        writeInt(prefix.prefixLength)
    }

    private fun Source.readUtf(): String {
        val length = readInt()
        if (length < 0) throw IOException("Invalid string length $length")
        return readString(length.toLong())
    }
}
