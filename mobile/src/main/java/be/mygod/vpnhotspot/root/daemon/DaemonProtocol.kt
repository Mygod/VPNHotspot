package be.mygod.vpnhotspot.root.daemon

import android.net.IpPrefix
import android.net.MacAddress
import android.net.Network
import be.mygod.vpnhotspot.util.CrashlyticsKeyProvider
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.MacAddressCompat
import com.google.firebase.crashlytics.CustomKeysAndValues
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
    const val FRAME_REPLY = 0
    const val FRAME_NEIGHBOURS = 1
    const val FRAME_NON_FATAL = 2

    const val STATUS_OK = 0
    const val STATUS_ERROR = 1

    private val INVALID_CRASHLYTICS_KEY_CHAR = Regex("[^A-Za-z0-9_.-]")

    private fun sanitizeCrashlyticsKey(key: String) =
        INVALID_CRASHLYTICS_KEY_CHAR.replace(key, "_").ifEmpty { "detail" }.take(64)

    data class DaemonErrorReport(
        val context: String,
        val message: String,
        val errno: Int?,
        val kind: String,
        val file: String,
        val line: Int,
        val column: Int,
        val pid: Int,
        val details: Map<String, String>,
    ) {
        val crashlyticsKeyValues get() = buildMap {
            put("daemon.context", context)
            put("daemon.message", message)
            if (errno != null) put("daemon.errno", errno.toString())
            put("daemon.kind", kind)
            put("daemon.file", file)
            put("daemon.line", line.toString())
            put("daemon.column", column.toString())
            put("daemon.pid", pid.toString())
            for ((key, value) in details) put("daemon.${sanitizeCrashlyticsKey(key)}", value)
        }
    }

    class DaemonException(
        val report: DaemonErrorReport,
    ) : IOException(report.toExceptionMessage()), CrashlyticsKeyProvider {
        init {
            stackTrace = arrayOf(StackTraceElement("vpnhotspotd", report.context, report.file, report.line))
        }

        override val crashlyticsKeys get() = CustomKeysAndValues.Builder().apply {
            for ((key, value) in report.crashlyticsKeyValues) putString(key, value)
        }.build()
    }

    const val CMD_START_SESSION = 1
    const val CMD_REPLACE_SESSION = 2
    const val CMD_REMOVE_SESSION = 3
    const val CMD_SHUTDOWN = 4
    const val CMD_READ_TRAFFIC_COUNTERS = 5
    const val CMD_START_NEIGHBOUR_MONITOR = 6
    const val CMD_STOP_NEIGHBOUR_MONITOR = 7
    const val CMD_DUMP_NEIGHBOURS = 8
    const val CMD_STATIC_ADDRESS = 9
    const val CMD_CLEAN_ROUTING = 12

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

    data class NeighbourUpdate(
        val replace: Boolean,
        val neighbours: List<NetlinkNeighbour>,
    )

    sealed class Frame {
        data class Reply(val packet: ByteArray) : Frame()
        data class Neighbours(val update: NeighbourUpdate) : Frame()
        data class NonFatal(val exception: DaemonException) : Frame()
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
    fun stopNeighbourMonitor() = writePacket(CMD_STOP_NEIGHBOUR_MONITOR) { }
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

    fun readFrame(packet: ByteArray): Frame {
        val input = Buffer().apply { write(packet) }
        return when (val type = input.readByte().toInt() and 0xFF) {
            FRAME_REPLY -> Frame.Reply(input.readByteArray())
            FRAME_NEIGHBOURS -> Frame.Neighbours(readNeighbourUpdate(input))
            FRAME_NON_FATAL -> Frame.NonFatal(DaemonException(readErrorReport(input)))
            else -> throw IOException("Unknown daemon frame type $type")
        }
    }

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

    fun readTrafficCounterLines(packet: ByteArray): List<String> {
        val input = Buffer().apply { write(packet) }
        readStatus(input)
        return List(input.readCount("traffic counter line")) { input.readUtf() }
    }

    fun readNeighbours(packet: ByteArray): List<NetlinkNeighbour> {
        val input = Buffer().apply { write(packet) }
        readStatus(input)
        return readNeighbours(input)
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

    private fun readStatus(input: Source) {
        when (val status = input.readByte().toInt() and 0xFF) {
            STATUS_OK -> { }
            STATUS_ERROR -> throw DaemonException(readErrorReport(input))
            else -> throw IOException("Unknown daemon status $status")
        }
    }

    private fun readErrorReport(input: Source): DaemonErrorReport {
        val context = input.readUtf()
        val message = input.readUtf()
        val errno = input.readInt().let { if (it < 0) null else it }
        val kind = input.readUtf()
        val file = input.readUtf()
        val line = input.readInt()
        val column = input.readInt()
        val pid = input.readInt()
        val detailCount = input.readCount("error detail")
        return DaemonErrorReport(context, message, errno, kind, file, line, column, pid, buildMap {
            repeat(detailCount) { put(input.readUtf(), input.readUtf()) }
        })
    }

    private fun DaemonErrorReport.toExceptionMessage() = buildString {
        append(context).append(": ").append(message)
        if (errno != null) append(" (errno=").append(errno).append(')')
        append(" [").append(kind).append(" at ").append(file).append(':').append(line).append(':')
            .append(column).append(", pid=").append(pid).append(']')
    }

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

    private fun readNeighbourUpdate(input: Source) =
            NeighbourUpdate(input.readByte() != 0.toByte(), readNeighbours(input))

    private fun readNeighbours(input: Source): List<NetlinkNeighbour> {
        val states = NetlinkNeighbour.State.values()
        return List(input.readCount("neighbour")) {
            val state = input.readByte().toInt() and 0xFF
            if (state !in states.indices) throw IOException("Invalid neighbour state $state")
            val ip = InetAddress.getByAddress(input.readByteArray(input.readCount("address byte")))
            val dev = input.readUtf()
            val lladdr = input.readByteArray(input.readCount("link-layer address byte")).let {
                if (it.size == 6) MacAddress.fromBytes(it) else MacAddressCompat.ALL_ZEROS_ADDRESS
            }
            NetlinkNeighbour(ip, dev, lladdr, states[state])
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
