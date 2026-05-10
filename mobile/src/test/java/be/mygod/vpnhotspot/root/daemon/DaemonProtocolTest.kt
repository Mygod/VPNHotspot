package be.mygod.vpnhotspot.root.daemon

import be.mygod.vpnhotspot.net.Routing
import kotlinx.io.Buffer
import kotlinx.io.Source
import kotlinx.io.readByteArray
import kotlinx.io.readString
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.net.InetAddress

class DaemonProtocolTest {
    @Test
    fun startSessionEncodesSessionConfig() {
        val packet = DaemonProtocol.startSession(DaemonProtocol.SessionConfig(
            downstream = "wlan0",
            ipForward = true,
            masquerade = Routing.MasqueradeMode.Simple,
            ipv6Block = false,
            primaryNetwork = null,
            primaryRoutes = emptyList(),
            fallbackNetwork = null,
            upstreams = listOf(DaemonProtocol.UpstreamConfig(
                DaemonProtocol.UpstreamRole.Primary, "rmnet_data0")),
            clients = emptyList(),
            ipv6Nat = null,
        )).packet
        Buffer().apply { write(packet) }.let { input ->
            assertEquals(DaemonProtocol.CMD_START_SESSION, input.readInt())
            assertEquals("wlan0", input.readUtf())
            assertEquals(true, input.readBoolean())
            assertEquals(Routing.MasqueradeMode.Simple.protocolValue, input.readByte())
            assertEquals(false, input.readBoolean())
            assertEquals(0L, input.readLong())
            assertEquals(0, input.readInt())
            assertEquals(0L, input.readLong())
            assertEquals(1, input.readInt())
            assertEquals(DaemonProtocol.UpstreamRole.Primary.protocolValue, input.readByte())
            assertEquals("rmnet_data0", input.readUtf())
            assertEquals(0, input.readInt())
            assertEquals(false, input.readBoolean())
        }
    }

    @Test
    fun replaceSessionEncodesSessionIdAndConfig() {
        val packet = DaemonProtocol.replaceSession(42, DaemonProtocol.SessionConfig(
            downstream = "wlan0",
            ipForward = true,
            masquerade = Routing.MasqueradeMode.Simple,
            ipv6Block = false,
            primaryNetwork = null,
            primaryRoutes = emptyList(),
            fallbackNetwork = null,
            upstreams = emptyList(),
            clients = emptyList(),
            ipv6Nat = null,
        )).packet
        Buffer().apply { write(packet) }.let { input ->
            assertEquals(DaemonProtocol.CMD_REPLACE_SESSION, input.readInt())
            assertEquals(42L, input.readLong())
            assertEquals("wlan0", input.readUtf())
        }
    }

    @Test
    fun startSessionEncodesIpv6NatPrefixSeed() {
        val packet = DaemonProtocol.startSession(DaemonProtocol.SessionConfig(
            downstream = "wlan0",
            ipForward = false,
            masquerade = Routing.MasqueradeMode.None,
            ipv6Block = false,
            primaryNetwork = null,
            primaryRoutes = emptyList(),
            fallbackNetwork = null,
            upstreams = emptyList(),
            clients = emptyList(),
            ipv6Nat = DaemonProtocol.Ipv6NatConfig("be.mygod.vpnhotspot\u0000android-id"),
        )).packet
        Buffer().apply { write(packet) }.let { input ->
            assertEquals(DaemonProtocol.CMD_START_SESSION, input.readInt())
            assertEquals("wlan0", input.readUtf())
            input.skip((1 + 1 + 1 + 8 + 4 + 8 + 4 + 4).toLong())
            assertEquals(true, input.readBoolean())
            assertEquals("be.mygod.vpnhotspot\u0000android-id", input.readUtf())
        }
    }

    @Test
    fun readFrameDecodesStructuredError() {
        val report = daemonErrorReport()
        val frame = DaemonTransport.readFrame(Buffer().apply {
            writeByte(2)
            writeLong(123)
            writeErrorReport(report)
        }.readByteArray())
        assertTrue(frame is DaemonTransport.Frame.Error)
        val e = (frame as DaemonTransport.Frame.Error).exception
        assertEquals(16, e.report.errno)
        assertEquals(report, e.report)
        assertDaemonReportCause(e, "routing.command: Device or resource busy (errno=16) " +
                "[ResourceBusy at routing.rs:123:45, pid=2345]")
        assertEquals(setOf("daemon.command"), e.report.crashlyticsKeyValues.keys)
        assertEquals("iptables-restore", e.report.crashlyticsKeyValues["daemon.command"])
    }

    @Test
    fun daemonExceptionMessageHidesUncategorizedWhenErrnoIsPresent() {
        val e = DaemonTransport.DaemonException(daemonErrorReport(
            message = "No such device",
            errno = 19,
            kind = "Uncategorized",
        ))

        assertEquals("routing.command: No such device (errno=19) [routing.rs:123:45, pid=2345]",
            e.cause!!.message)
        assertEquals("Uncategorized", e.report.kind)
    }

    @Test
    fun readFrameDecodesNonFatalError() {
        val report = daemonErrorReport()
        val frame = DaemonTransport.readFrame(Buffer().apply {
            writeByte(3)
            writeLong(0)
            writeErrorReport(report)
        }.readByteArray())
        assertTrue(frame is DaemonTransport.Frame.NonFatal)
        val e = (frame as DaemonTransport.Frame.NonFatal).exception
        assertEquals(report, e.report)
        assertDaemonReportCause(e)
    }

    @Test
    fun readFrameDecodesReplyAndEventIds() {
        val reply = DaemonTransport.readFrame(Buffer().apply {
            writeByte(0)
            writeLong(10)
            writeByte(1)
        }.readByteArray())
        assertTrue(reply is DaemonTransport.Frame.Reply)
        assertEquals(10, (reply as DaemonTransport.Frame.Reply).id)
        assertEquals(1, reply.packet.single().toInt())
        val event = DaemonTransport.readFrame(Buffer().apply {
            writeByte(1)
            writeLong(11)
            writeByte(2)
        }.readByteArray())
        assertTrue(event is DaemonTransport.Frame.Event)
        assertEquals(11, (event as DaemonTransport.Frame.Event).id)
        assertEquals(2, event.packet.single().toInt())
        val complete = DaemonTransport.readFrame(Buffer().apply {
            writeByte(4)
            writeLong(12)
        }.readByteArray())
        assertTrue(complete is DaemonTransport.Frame.Complete)
        assertEquals(12, (complete as DaemonTransport.Frame.Complete).id)
    }

    @Test
    fun readNeighbourDeltasDecodesEmptyList() {
        val deltas = DaemonProtocol.readNeighbourDeltas(Buffer().apply { writeInt(0) }.readByteArray())

        assertEquals(emptyList<DaemonProtocol.NeighbourDelta>(), deltas)
    }

    @Test
    fun readNeighbourDeltasDecodesNullMacAndDelete() {
        val packet = Buffer().apply {
            writeInt(2)
            writeByte(0)
            writeByte(2)
            writeInetAddress(InetAddress.getByName("192.0.2.2"))
            writeUtf("wlan0")
            writeByte(0)
            writeByte(1)
            writeInetAddress(InetAddress.getByName("2001:db8::1"))
            writeUtf("wlan1")
        }.readByteArray()

        val deltas = DaemonProtocol.readNeighbourDeltas(packet)
        val upsert = deltas[0] as DaemonProtocol.NeighbourDelta.Upsert
        assertEquals("192.0.2.2", upsert.neighbour.ip.hostAddress)
        assertEquals("wlan0", upsert.neighbour.dev)
        assertEquals(null, upsert.neighbour.lladdr)
        assertEquals(be.mygod.vpnhotspot.net.NetlinkNeighbour.State.VALID, upsert.neighbour.state)
        val delete = deltas[1] as DaemonProtocol.NeighbourDelta.Delete
        assertEquals("2001:db8:0:0:0:0:0:1", delete.ip.hostAddress)
        assertEquals("wlan1", delete.dev)
    }

    @Test
    fun transportCallAndCancelCommandEncodeCallId() {
        Buffer().apply {
            write(DaemonTransport.call(123, byteArrayOf(1, 2)))
        }.let { input ->
            assertEquals(123, input.readLong())
            assertEquals(1, input.readByte().toInt())
            assertEquals(2, input.readByte().toInt())
        }
        Buffer().apply {
            write(DaemonTransport.call(124, DaemonProtocol.cancel().packet))
        }.let { input ->
            assertEquals(124, input.readLong())
            assertEquals(DaemonProtocol.CMD_CANCEL, input.readInt())
        }
    }

    @Test
    fun cleanRoutingEncodesPrefixSeed() {
        Buffer().apply {
            write(DaemonProtocol.cleanRouting("be.mygod.vpnhotspot\u0000android-id").packet)
        }.let { input ->
            assertEquals(DaemonProtocol.CMD_CLEAN_ROUTING, input.readInt())
            assertEquals("be.mygod.vpnhotspot\u0000android-id", input.readUtf())
        }
    }

    private fun Source.readBoolean() = readByte().toInt() != 0

    private fun Source.readUtf() = readString(readInt().toLong())

    private fun daemonErrorReport(
        message: String = "Device or resource busy",
        errno: Int? = 16,
        kind: String = "ResourceBusy",
    ) = DaemonTransport.DaemonErrorReport(
        context = "routing.command",
        message = message,
        errno = errno,
        kind = kind,
        file = "routing.rs",
        line = 123,
        column = 45,
        pid = 2345,
        details = mapOf("command" to "iptables-restore"),
    )

    private fun assertDaemonReportCause(e: DaemonTransport.DaemonException, message: String? = null) {
        assertNotNull(e.cause)
        if (message != null) assertEquals(message, e.cause!!.message)
        val frame = e.cause!!.stackTrace.single()
        assertEquals("vpnhotspotd", frame.className)
        assertEquals("routing.command", frame.methodName)
        assertEquals("routing.rs", frame.fileName)
        assertEquals(123, frame.lineNumber)
    }

    private fun Buffer.writeErrorReport(report: DaemonTransport.DaemonErrorReport) {
        writeUtf(report.context)
        writeUtf(report.message)
        writeInt(report.errno ?: -1)
        writeUtf(report.kind)
        writeUtf(report.file)
        writeInt(report.line)
        writeInt(report.column)
        writeInt(report.pid)
        writeInt(report.details.size)
        for ((key, value) in report.details) {
            writeUtf(key)
            writeUtf(value)
        }
    }

    private fun Buffer.writeUtf(value: String) {
        val bytes = value.encodeToByteArray()
        writeInt(bytes.size)
        write(bytes)
    }

    private fun Buffer.writeInetAddress(address: InetAddress) {
        writeInt(address.address.size)
        write(address.address)
    }
}
