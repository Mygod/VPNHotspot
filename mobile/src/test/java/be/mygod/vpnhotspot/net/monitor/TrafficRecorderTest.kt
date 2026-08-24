package be.mygod.vpnhotspot.net.monitor

import be.mygod.vpnhotspot.root.daemon.DaemonTrafficSource
import be.mygod.vpnhotspot.root.daemon.TrafficCounter
import be.mygod.vpnhotspot.root.daemon.TrafficCounterSource
import be.mygod.vpnhotspot.room.TrafficRecord
import okio.ByteString.Companion.toByteString
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import java.net.InetAddress

class TrafficRecorderTest {
    @Test
    fun counterSourceMapsIpv4ForwardAddressToRealIpRow() {
        val source = TrafficRecorder.counterSource(
            TrafficCounter(
                source = TrafficCounterSource(
                    ipv4_forward_address = byteArrayOf(192.toByte(), 0, 2, 8).toByteString(),
                ),
            ),
        )
        assertEquals(InetAddress.getByAddress(byteArrayOf(192.toByte(), 0, 2, 8)), source?.ip)
        assertNull(source?.upstream)
    }

    @Test
    fun counterSourceRejectsNonIpv4ForwardAddress() {
        assertNull(
            TrafficRecorder.counterSource(
                TrafficCounter(
                    source = TrafficCounterSource(
                        ipv4_forward_address = ByteArray(16).toByteString(),
                    ),
                ),
            ),
        )
        assertNull(
            TrafficRecorder.counterSource(
                TrafficCounter(
                    source = TrafficCounterSource(
                        ipv4_forward_address = ByteArray(3).toByteString(),
                    ),
                ),
            ),
        )
    }

    @Test
    fun counterSourceMapsDaemonSourcesToMarkerRows() {
        for ((source, marker) in listOf(
            DaemonTrafficSource.DAEMON_TRAFFIC_SOURCE_DNS to TrafficRecord.DAEMON_SOURCE_DNS,
            DaemonTrafficSource.DAEMON_TRAFFIC_SOURCE_NAT66_TCP to TrafficRecord.DAEMON_SOURCE_NAT66_TCP,
            DaemonTrafficSource.DAEMON_TRAFFIC_SOURCE_NAT66_UDP to TrafficRecord.DAEMON_SOURCE_NAT66_UDP,
            DaemonTrafficSource.DAEMON_TRAFFIC_SOURCE_NAT66_ICMPV6 to TrafficRecord.DAEMON_SOURCE_NAT66_ICMPV6,
        )) {
            val recordSource = TrafficRecorder.counterSource(
                TrafficCounter(source = TrafficCounterSource(daemon_source = source)),
            )
            assertEquals(TrafficRecord.DAEMON_SOURCE_ADDRESS, recordSource?.ip)
            assertEquals(marker, recordSource?.upstream)
        }
    }

    @Test
    fun counterSourceRejectsUnspecifiedDaemonSource() {
        assertNull(
            TrafficRecorder.counterSource(
                TrafficCounter(
                    source = TrafficCounterSource(
                        daemon_source = DaemonTrafficSource.DAEMON_TRAFFIC_SOURCE_UNSPECIFIED,
                    ),
                ),
            ),
        )
    }

    @Test
    fun counterKeyRejectsInvalidMacLength() {
        assertNull(
            TrafficRecorder.counterKey(
                TrafficCounter(
                    mac = ByteArray(5).toByteString(),
                    downstream = "ncm0",
                    source = TrafficCounterSource(
                        daemon_source = DaemonTrafficSource.DAEMON_TRAFFIC_SOURCE_DNS,
                    ),
                ),
            ),
        )
    }
}
