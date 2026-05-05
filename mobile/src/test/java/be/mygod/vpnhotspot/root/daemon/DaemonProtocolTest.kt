package be.mygod.vpnhotspot.root.daemon

import kotlinx.io.Buffer
import kotlinx.io.Source
import kotlinx.io.readByteArray
import kotlinx.io.readString
import org.junit.Assert.assertEquals
import org.junit.Test
import java.net.Inet4Address
import java.net.InetAddress

class DaemonProtocolTest {
    @Test
    fun startSessionEncodesSessionConfig() {
        val packet = DaemonProtocol.startSession(DaemonProtocol.SessionConfig(
            downstream = "wlan0",
            dnsBindAddress = InetAddress.getByName("192.0.2.1") as Inet4Address,
            downstreamPrefixLength = 24,
            ipForward = true,
            forward = true,
            masquerade = DaemonProtocol.MasqueradeMode.Simple,
            ipv6Block = false,
            primaryNetwork = null,
            primaryRoutes = emptyList(),
            fallbackNetwork = null,
            upstreams = listOf(DaemonProtocol.UpstreamConfig(
                DaemonProtocol.UpstreamRole.Primary, "rmnet_data0", 1234)),
            clients = emptyList(),
            ipv6Nat = null,
        ))
        Buffer().apply { write(packet) }.let { input ->
            assertEquals(DaemonProtocol.CMD_START_SESSION, input.readInt())
            assertEquals("wlan0", input.readUtf())
            assertEquals("192.0.2.1", input.readIpv4Address())
            assertEquals(24, input.readInt())
            assertEquals(true, input.readBoolean())
            assertEquals(true, input.readBoolean())
            assertEquals(DaemonProtocol.MasqueradeMode.Simple.protocolValue, input.readByte())
            assertEquals(false, input.readBoolean())
            assertEquals(0L, input.readLong())
            assertEquals(0, input.readInt())
            assertEquals(0L, input.readLong())
            assertEquals(1, input.readInt())
            assertEquals(DaemonProtocol.UpstreamRole.Primary.protocolValue, input.readByte())
            assertEquals("rmnet_data0", input.readUtf())
            assertEquals(1234, input.readInt())
            assertEquals(0, input.readInt())
            assertEquals(false, input.readBoolean())
        }
    }

    @Test
    fun startSessionEncodesIpv6NatPrefixSeed() {
        val packet = DaemonProtocol.startSession(DaemonProtocol.SessionConfig(
            downstream = "wlan0",
            dnsBindAddress = InetAddress.getByName("192.0.2.1") as Inet4Address,
            downstreamPrefixLength = 24,
            ipForward = false,
            forward = false,
            masquerade = DaemonProtocol.MasqueradeMode.None,
            ipv6Block = false,
            primaryNetwork = null,
            primaryRoutes = emptyList(),
            fallbackNetwork = null,
            upstreams = emptyList(),
            clients = emptyList(),
            ipv6Nat = DaemonProtocol.Ipv6NatConfig("be.mygod.vpnhotspot\u0000android-id", 1280,
                emptyList(), emptyList()),
        ))
        Buffer().apply { write(packet) }.let { input ->
            assertEquals(DaemonProtocol.CMD_START_SESSION, input.readInt())
            assertEquals("wlan0", input.readUtf())
            input.skip((4 + 4 + 1 + 1 + 1 + 1 + 8 + 4 + 8 + 4 + 4).toLong())
            assertEquals(true, input.readBoolean())
            assertEquals("be.mygod.vpnhotspot\u0000android-id", input.readUtf())
            assertEquals(1280, input.readInt())
            assertEquals(0, input.readInt())
            assertEquals(0, input.readInt())
        }
    }

    @Test
    fun readPortsDecodesResponse() {
        val packet = byteArrayOf(
            DaemonProtocol.STATUS_OK.toByte(),
            0x12, 0x34,
            0x23, 0x45,
            1,
            0x34, 0x56,
            0x45, 0x67,
        )
        assertEquals(DaemonProtocol.SessionPorts(0x1234, 0x2345,
            DaemonProtocol.Ipv6NatPorts(0x3456, 0x4567)), DaemonProtocol.readPorts(packet))
    }

    @Test
    fun removeSessionEncodesRemoveMode() {
        Buffer().apply {
            write(DaemonProtocol.removeSession("wlan0", DaemonProtocol.RemoveMode.WithdrawCleanup))
        }.let { input ->
            assertEquals(DaemonProtocol.CMD_REMOVE_SESSION, input.readInt())
            assertEquals("wlan0", input.readUtf())
            assertEquals(DaemonProtocol.RemoveMode.WithdrawCleanup.protocolValue, input.readByte())
        }
    }

    @Test
    fun shutdownEncodesRemoveMode() {
        Buffer().apply {
            write(DaemonProtocol.shutdown(DaemonProtocol.RemoveMode.PreserveCleanup))
        }.let { input ->
            assertEquals(DaemonProtocol.CMD_SHUTDOWN, input.readInt())
            assertEquals(DaemonProtocol.RemoveMode.PreserveCleanup.protocolValue, input.readByte())
        }
    }

    @Test
    fun cleanRoutingEncodesPrefixSeed() {
        Buffer().apply {
            write(DaemonProtocol.cleanRouting("be.mygod.vpnhotspot\u0000android-id"))
        }.let { input ->
            assertEquals(DaemonProtocol.CMD_CLEAN_ROUTING, input.readInt())
            assertEquals("be.mygod.vpnhotspot\u0000android-id", input.readUtf())
        }
    }

    private fun Source.readBoolean() = readByte().toInt() != 0

    private fun Source.readIpv4Address() = InetAddress.getByAddress(readByteArray(4)).hostAddress

    private fun Source.readUtf() = readString(readInt().toLong())
}
