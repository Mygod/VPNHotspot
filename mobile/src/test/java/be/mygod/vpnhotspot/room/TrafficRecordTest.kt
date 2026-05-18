package be.mygod.vpnhotspot.room

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class TrafficRecordTest {
    @Test
    fun clientStatsRowMapsKnownMarkersToStatsSources() {
        for ((marker, source) in listOf(
            null to TrafficStatsSource.IPV4,
            TrafficRecord.DAEMON_SOURCE_DNS to TrafficStatsSource.DNS,
            TrafficRecord.DAEMON_SOURCE_NAT66_TCP to TrafficStatsSource.NAT66_TCP,
            TrafficRecord.DAEMON_SOURCE_NAT66_UDP to TrafficStatsSource.NAT66_UDP,
            TrafficRecord.DAEMON_SOURCE_NAT66_ICMPV6 to TrafficStatsSource.NAT66_ICMPV6,
        )) {
            val stats = ClientStatsRow(
                marker = marker,
                timestamp = 5,
                sentPackets = 1,
                sentBytes = 2,
                receivedPackets = 3,
                receivedBytes = 4,
            ).toStats()

            assertEquals(source, stats?.source)
            assertEquals(5L, stats?.timestamp)
            assertEquals(1L, stats?.sentPackets)
            assertEquals(2L, stats?.sentBytes)
            assertEquals(3L, stats?.receivedPackets)
            assertEquals(4L, stats?.receivedBytes)
            assertEquals(true, stats?.connectionCountKnown)
        }
    }

    @Test
    fun clientStatsRowRejectsUnknownDaemonMarker() {
        assertNull(ClientStatsRow(marker = "/unknown").toStats())
    }

    @Test
    fun clientStatsRowMarksLegacyNat66TcpConnectionCountUnknown() {
        val stats = ClientStatsRow(
            marker = TrafficRecord.DAEMON_SOURCE_NAT66_TCP,
            sentBytes = 1,
            hasLegacyNat66TcpRows = true,
        ).toStats()

        assertEquals(TrafficStatsSource.NAT66_TCP, stats?.source)
        assertEquals(false, stats?.connectionCountKnown)
    }

    @Test
    fun clientStatsTimestampReturnsFirstEntryTimestamp() {
        assertEquals(0L, ClientStats().timestamp)
        assertEquals(3L, ClientStats(listOf(
            ClientStatsEntry(TrafficStatsSource.DNS, timestamp = 5),
            ClientStatsEntry(TrafficStatsSource.IPV4, timestamp = 3),
        )).timestamp)
    }
}
