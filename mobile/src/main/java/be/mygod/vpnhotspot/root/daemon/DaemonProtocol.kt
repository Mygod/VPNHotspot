package be.mygod.vpnhotspot.root.daemon

import android.net.IpPrefix
import android.net.MacAddress
import android.net.Network
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.Routing
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
    const val CMD_READ_TRAFFIC_COUNTERS = 5
    const val CMD_START_NEIGHBOUR_MONITOR = 6
    const val CMD_CLEAN_ROUTING = 12
    const val CMD_REPLACE_STATIC_ADDRESSES = 13
    const val CMD_DELETE_STATIC_ADDRESSES = 14

    private const val NEIGHBOUR_DELTA_UPSERT = 0
    private const val NEIGHBOUR_DELTA_DELETE = 1

    data class SessionConfig(
        val downstream: String,
        val ipForward: Boolean,
        val masquerade: Routing.MasqueradeMode,
        val ipv6Block: Boolean,
        val primaryNetwork: Network?,
        val primaryRoutes: List<IpPrefix>,
        val fallbackNetwork: Network?,
        val upstreams: List<UpstreamConfig>,
        val clients: List<ClientConfig>,
        val ipv6Nat: Ipv6NatConfig?,
    )

    enum class UpstreamRole(val protocolValue: Byte) {
        Primary(0),
        Fallback(1),
    }

    data class UpstreamConfig(
        val role: UpstreamRole,
        val ifname: String,
    )

    data class ClientConfig(
        val mac: MacAddress,
        val ipv4: List<Inet4Address>,
    )

    data class Ipv6NatConfig(
        val prefixSeed: String,
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

    class Command internal constructor(val packet: ByteArray, private val description: String) {
        override fun toString() = description
    }

    fun startSession(config: SessionConfig) = writePacket(CMD_START_SESSION, "StartSession(config=$config)") {
        writeSession(config)
    }
    fun replaceSession(config: SessionConfig) = writePacket(CMD_REPLACE_SESSION, "ReplaceSession(config=$config)") {
        writeSession(config)
    }
    fun removeSession(downstream: String, mode: RemoveMode) = writePacket(CMD_REMOVE_SESSION,
            "RemoveSession(downstream=$downstream, mode=$mode)") {
        writeUtf(downstream)
        writeByte(mode.protocolValue)
    }
    fun readTrafficCounters() = writePacket(CMD_READ_TRAFFIC_COUNTERS, "ReadTrafficCounters") { }
    fun startNeighbourMonitor() = writePacket(CMD_START_NEIGHBOUR_MONITOR, "StartNeighbourMonitor") { }
    fun replaceStaticAddresses(dev: String, addresses: List<Pair<InetAddress, Int>>) =
            writePacket(CMD_REPLACE_STATIC_ADDRESSES, "ReplaceStaticAddresses(dev=$dev, addresses=$addresses)") {
                writeUtf(dev)
                writeInt(addresses.size)
                for ((address, prefixLength) in addresses) {
                    writeInetAddress(address)
                    writeInt(prefixLength)
                }
            }
    fun deleteStaticAddresses(dev: String) = writePacket(CMD_DELETE_STATIC_ADDRESSES,
            "DeleteStaticAddresses(dev=$dev)") {
        writeUtf(dev)
    }
    fun cleanRouting(ipv6NatPrefixSeed: String) = writePacket(CMD_CLEAN_ROUTING,
            "CleanRouting(ipv6NatPrefixSeed=$ipv6NatPrefixSeed)") {
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

    fun readNeighbourDeltas(packet: ByteArray): List<NeighbourDelta> {
        return readNeighbourDeltas(Buffer().apply { write(packet) })
    }

    private fun writePacket(command: Int, description: String, block: Sink.() -> Unit): Command {
        val output = Buffer()
        output.writeInt(command)
        output.block()
        return Command(output.readByteArray(), description)
    }

    private fun Sink.writeSession(config: SessionConfig) {
        writeUtf(config.downstream)
        writeByte((if (config.ipForward) 1 else 0).toByte())
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
