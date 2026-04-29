package be.mygod.vpnhotspot.root.daemon

import kotlinx.io.Buffer
import kotlinx.io.Source
import kotlinx.io.readString
import org.junit.Assert.assertEquals
import org.junit.Test

class DaemonProtocolTest {
    @Test
    fun startSessionEncodesSessionConfig() {
        val packet = DaemonProtocol.startSession(DaemonProtocol.SessionConfig(
            sessionId = "wlan0",
            generationId = 7,
            downstream = "wlan0",
            router = "fe80::1",
            gateway = "fd00:1234:5678:9abc::1",
            prefixLength = 64,
            replyMark = 0x71d8,
            dnsBindAddress = "127.0.0.1",
            mtu = 1440,
            suppressedPrefixes = listOf(
                DaemonProtocol.Route("2600:db8::59", 64),
                DaemonProtocol.Route("fd00:dead:beef::1", 64),
            ),
            cleanupPrefixes = listOf(DaemonProtocol.Route("fd00:cafe::1", 64)),
            primary = DaemonProtocol.Upstream(
                networkHandle = 123L,
                interfaceName = "tun0",
                dnsServers = listOf("2001:4860:4860::8888"),
                routes = listOf(
                    DaemonProtocol.Route("::", 0),
                    DaemonProtocol.Route("2001:db8::", 32),
                ),
            ),
            fallback = DaemonProtocol.Upstream(
                networkHandle = 456L,
                interfaceName = "tun1",
                dnsServers = emptyList(),
                routes = listOf(DaemonProtocol.Route("2001:db8:ffff::", 48)),
            ),
        ))
        Buffer().apply { write(packet) }.let { input ->
            assertEquals(DaemonProtocol.CMD_START_SESSION, input.readInt())
            assertEquals("wlan0", input.readUtf())
            assertEquals(7, input.readInt())
            assertEquals("wlan0", input.readUtf())
            assertEquals("fe80::1", input.readUtf())
            assertEquals("fd00:1234:5678:9abc::1", input.readUtf())
            assertEquals(64, input.readInt())
            assertEquals(0x71d8, input.readInt())
            assertEquals("127.0.0.1", input.readUtf())
            assertEquals(1440, input.readInt())
            assertEquals(2, input.readInt())
            assertEquals("2600:db8::59", input.readUtf())
            assertEquals(64, input.readInt())
            assertEquals("fd00:dead:beef::1", input.readUtf())
            assertEquals(64, input.readInt())
            assertEquals(1, input.readInt())
            assertEquals("fd00:cafe::1", input.readUtf())
            assertEquals(64, input.readInt())

            assertEquals(true, input.readBoolean())
            assertEquals(123L, input.readLong())
            assertEquals("tun0", input.readUtf())
            assertEquals(1, input.readInt())
            assertEquals("2001:4860:4860::8888", input.readUtf())
            assertEquals(2, input.readInt())
            assertEquals("::", input.readUtf())
            assertEquals(0, input.readInt())
            assertEquals("2001:db8::", input.readUtf())
            assertEquals(32, input.readInt())

            assertEquals(true, input.readBoolean())
            assertEquals(456L, input.readLong())
            assertEquals("tun1", input.readUtf())
            assertEquals(0, input.readInt())
            assertEquals(1, input.readInt())
            assertEquals("2001:db8:ffff::", input.readUtf())
            assertEquals(48, input.readInt())
        }
    }

    @Test
    fun readPortsDecodesResponse() {
        val packet = byteArrayOf(
            DaemonProtocol.STATUS_OK.toByte(),
            0x12, 0x34,
            0x23, 0x45,
            0x34, 0x56,
            0x45, 0x67,
        )
        assertEquals(DaemonProtocol.SessionPorts(0x1234, 0x2345, 0x3456, 0x4567),
            DaemonProtocol.readPorts(packet))
    }

    @Test
    fun removeSessionEncodesRemoveMode() {
        Buffer().apply {
            write(DaemonProtocol.removeSession("wlan0", DaemonProtocol.RemoveMode.WithdrawCleanup))
        }.let { input ->
            assertEquals(DaemonProtocol.CMD_REMOVE_SESSION, input.readInt())
            assertEquals("wlan0", input.readUtf())
            assertEquals(true, input.readBoolean())
        }
    }

    @Test
    fun shutdownEncodesRemoveMode() {
        Buffer().apply {
            write(DaemonProtocol.shutdown(DaemonProtocol.RemoveMode.PreserveCleanup))
        }.let { input ->
            assertEquals(DaemonProtocol.CMD_SHUTDOWN, input.readInt())
            assertEquals(false, input.readBoolean())
        }
    }

    private fun Source.readBoolean() = readByte().toInt() != 0

    private fun Source.readUtf() = readString((readShort().toInt() and 0xFFFF).toLong())
}
