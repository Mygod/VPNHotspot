package be.mygod.vpnhotspot.root.daemon

import android.net.IpPrefix
import android.net.MacAddress
import android.net.Network
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import kotlinx.io.Buffer
import kotlinx.io.Sink
import kotlinx.io.Source
import kotlinx.io.readByteArray
import kotlinx.io.readString
import java.io.IOException
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress

object DaemonProtocol {
    const val CMD_START_SESSION = 1
    const val CMD_REPLACE_SESSION = 2
    const val CMD_REMOVE_SESSION = 3
    const val CMD_SHUTDOWN = 4
    const val CMD_READ_TRAFFIC_COUNTERS = 5
    const val CMD_START_NEIGHBOUR_MONITOR = 6
    const val CMD_DUMP_NEIGHBOURS = 8
    const val CMD_STATIC_ADDRESS = 9
    const val CMD_CLEAN_ROUTING = 12

    private const val NEIGHBOUR_DELTA_UPSERT = 0
    private const val NEIGHBOUR_DELTA_DELETE = 1

    data class SessionConfig(
        val downstream: String,
        val dnsBindAddress: Inet4Address,
        val downstreamPrefixLength: Int,
        val ipForward: Boolean,
        val forward: Boolean,
        val masquerade: MasqueradeMode,
        val ipv6Block: Boolean,
        val primaryNetwork: Network?,
        val primaryRoutes: List<IpPrefix>,
        val fallbackNetwork: Network?,
        val upstreams: List<UpstreamConfig>,
        val clients: List<ClientConfig>,
        val ipv6Nat: Ipv6NatConfig?,
    )

    enum class MasqueradeMode(val protocolValue: Byte) {
        None(0),
        Simple(1),
        Netd(2),
    }

    enum class UpstreamRole(val protocolValue: Byte) {
        Primary(0),
        Fallback(1),
    }

    data class UpstreamConfig(
        val role: UpstreamRole,
        val ifname: String,
        val ifindex: Int,
    )

    data class ClientConfig(
        val mac: MacAddress,
        val ipv4: List<Inet4Address>,
    )

    data class Ipv6NatConfig(
        val prefixSeed: String,
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

    sealed class NeighbourDelta {
        data class Upsert(val neighbour: NetlinkNeighbour) : NeighbourDelta()
        data class Delete(val ip: InetAddress, val dev: String) : NeighbourDelta()
    }

    enum class RemoveMode(val protocolValue: Byte) {
        PreserveCleanup(0),
        WithdrawCleanup(1),
    }

    enum class IpOperation(val protocolValue: Byte) {
        Replace(0),
        Delete(1),
    }

    fun startSession(config: SessionConfig) = writePacket(CMD_START_SESSION) { writeSession(config) }
    fun replaceSession(config: SessionConfig) = writePacket(CMD_REPLACE_SESSION) { writeSession(config) }
    fun removeSession(downstream: String, mode: RemoveMode) = writePacket(CMD_REMOVE_SESSION) {
        writeUtf(downstream)
        writeByte(mode.protocolValue)
    }
    fun shutdown(mode: RemoveMode) = writePacket(CMD_SHUTDOWN) { writeByte(mode.protocolValue) }
    fun readTrafficCounters() = writePacket(CMD_READ_TRAFFIC_COUNTERS) { }
    fun startNeighbourMonitor() = writePacket(CMD_START_NEIGHBOUR_MONITOR) { }
    fun dumpNeighbours() = writePacket(CMD_DUMP_NEIGHBOURS) { }
    fun staticAddress(operation: IpOperation, address: InetAddress, prefixLength: Int, dev: String) =
            writePacket(CMD_STATIC_ADDRESS) {
                writeByte(operation.protocolValue)
                writeInetAddress(address)
                writeInt(prefixLength)
                writeUtf(dev)
            }
    fun cleanRouting(ipv6NatPrefixSeed: String) = writePacket(CMD_CLEAN_ROUTING) {
        writeUtf(ipv6NatPrefixSeed)
    }

    fun readPorts(packet: ByteArray): SessionPorts {
        val input = Buffer().apply { write(packet) }
        return SessionPorts(
            input.readShort().toUShort().toInt(),
            input.readShort().toUShort().toInt(),
            if (input.readByte() != 0.toByte()) {
                Ipv6NatPorts(input.readShort().toUShort().toInt(), input.readShort().toUShort().toInt())
            } else null,
        )
    }

    fun readAck(packet: ByteArray) {
        if (packet.isNotEmpty()) throw IOException("Unexpected daemon ACK payload length ${packet.size}")
    }

    fun readTrafficCounterLines(packet: ByteArray): List<String> {
        val input = Buffer().apply { write(packet) }
        return List(input.readCount("traffic counter line")) { input.readUtf() }
    }

    fun readNeighbours(packet: ByteArray): List<NetlinkNeighbour> {
        return readNeighbourDeltas(packet).mapNotNull { (it as? NeighbourDelta.Upsert)?.neighbour }
    }

    fun readNeighbourDeltas(packet: ByteArray): List<NeighbourDelta> {
        return readNeighbourDeltas(Buffer().apply { write(packet) })
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
        writeInt(config.downstreamPrefixLength)
        writeByte((if (config.ipForward) 1 else 0).toByte())
        writeByte((if (config.forward) 1 else 0).toByte())
        writeByte(config.masquerade.protocolValue)
        writeByte((if (config.ipv6Block) 1 else 0).toByte())
        writeNetwork(config.primaryNetwork)
        writeInt(config.primaryRoutes.size)
        for (route in config.primaryRoutes) writeIpv6Prefix(route)
        writeNetwork(config.fallbackNetwork)
        writeInt(config.upstreams.size)
        for (upstream in config.upstreams) {
            writeByte(upstream.role.protocolValue)
            writeUtf(upstream.ifname)
            writeInt(upstream.ifindex)
        }
        writeInt(config.clients.size)
        for (client in config.clients) {
            write(client.mac.toByteArray())
            writeInt(client.ipv4.size)
            for (address in client.ipv4) writeInet4Address(address)
        }
        val ipv6Nat = config.ipv6Nat
        writeByte((if (ipv6Nat != null) 1 else 0).toByte())
        if (ipv6Nat == null) return
        writeUtf(ipv6Nat.prefixSeed)
        writeInt(ipv6Nat.mtu)
        writeInt(ipv6Nat.suppressedPrefixes.size)
        for (prefix in ipv6Nat.suppressedPrefixes) writeIpv6Prefix(prefix)
        writeInt(ipv6Nat.cleanupPrefixes.size)
        for (prefix in ipv6Nat.cleanupPrefixes) writeIpv6Prefix(prefix)
    }

    private fun Sink.writeNetwork(network: Network?) = writeLong(network?.networkHandle ?: 0L)

    private fun Sink.writeUtf(value: String) {
        val bytes = value.encodeToByteArray()
        writeInt(bytes.size)
        write(bytes)
    }

    private fun Sink.writeInet4Address(address: Inet4Address) = write(address.address)

    private fun Sink.writeInet6Address(address: Inet6Address) = write(address.address)

    private fun Sink.writeInetAddress(address: InetAddress) {
        writeInt(address.address.size)
        write(address.address)
    }

    private fun Sink.writeIpv6Prefix(prefix: IpPrefix) {
        val address = prefix.address
        require(address is Inet6Address) { "IPv6 prefix expected: $prefix" }
        writeInet6Address(address)
        writeInt(prefix.prefixLength)
    }

    private fun readNeighbourDeltas(input: Source): List<NeighbourDelta> {
        val states = NetlinkNeighbour.State.values()
        return List(input.readCount("neighbour")) {
            when (val type = input.readByte().toInt() and 0xFF) {
                NEIGHBOUR_DELTA_UPSERT -> {
                    val state = input.readByte().toInt() and 0xFF
                    if (state !in states.indices) throw IOException("Invalid neighbour state $state")
                    val ip = InetAddress.getByAddress(input.readByteArray(input.readCount("address byte")))
                    val dev = input.readUtf()
                    val lladdr = when (val present = input.readByte().toInt() and 0xFF) {
                        0 -> null
                        1 -> MacAddress.fromBytes(input.readByteArray(6))
                        else -> throw IOException("Invalid neighbour link-layer marker $present")
                    }
                    NeighbourDelta.Upsert(NetlinkNeighbour(ip, dev, lladdr, states[state]))
                }
                NEIGHBOUR_DELTA_DELETE ->
                    NeighbourDelta.Delete(InetAddress.getByAddress(input.readByteArray(input.readCount("address byte"))),
                        input.readUtf())
                else -> throw IOException("Invalid neighbour delta type $type")
            }
        }
    }

    private fun Source.readUtf(): String {
        val length = readInt()
        if (length < 0) throw IOException("Invalid string length $length")
        return readString(length.toLong())
    }

    private fun Source.readCount(name: String): Int {
        val count = readInt()
        if (count < 0) throw IOException("Invalid $name count $count")
        return count
    }
}
