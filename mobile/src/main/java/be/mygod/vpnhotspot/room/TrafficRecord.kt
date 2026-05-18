package be.mygod.vpnhotspot.room

import android.net.MacAddress
import android.os.Parcelable
import androidx.room.*
import kotlinx.parcelize.Parcelize
import java.net.InetAddress

@Entity(foreignKeys = [ForeignKey(entity = TrafficRecord::class, parentColumns = ["id"], childColumns = ["previousId"],
            onDelete = ForeignKey.CASCADE, onUpdate = ForeignKey.RESTRICT)],
        indices = [Index(value = ["previousId"], unique = true)])
data class TrafficRecord(
        /**
         * Setting id = null should only be used when a new row is created and not yet inserted into the database.
         *
         * https://www.sqlite.org/lang_createtable.html#primkeyconst:
         * > Unless the column is an INTEGER PRIMARY KEY or the table is a WITHOUT ROWID table or the column is declared
         * > NOT NULL, SQLite allows NULL values in a PRIMARY KEY column.
         */
        @PrimaryKey(autoGenerate = true)
        var id: Long? = null,
        val timestamp: Long = System.currentTimeMillis(),
        /**
         * Foreign key/ID for (possibly non-existent, i.e. default) entry in ClientRecord.
         */
        val mac: MacAddress,
        /**
         * IPv4 forwarding rows store the real client IPv4 address. Daemon-owned MAC-only counter sources use
         * 0.0.0.0 as a schema-compatibility marker.
         */
        val ip: InetAddress,
        /**
         * Compatibility column. Historical rows used this as an upstream interface name; daemon-owned MAC-only
         * counter sources store reserved source markers such as /dns or /nat66/tcp.
         */
        val upstream: String? = null,
        val downstream: String,
        var sentPackets: Long = 0,
        var sentBytes: Long = 0,
        var receivedPackets: Long = 0,
        var receivedBytes: Long = 0,
        /**
         * ID for the previous traffic record.
         */
        val previousId: Long? = null) {
    companion object {
        val DAEMON_SOURCE_ADDRESS: InetAddress = InetAddress.getByAddress(byteArrayOf(0, 0, 0, 0))
        const val DAEMON_SOURCE_DNS = "/dns"
        const val DAEMON_SOURCE_NAT66_TCP = "/nat66/tcp"
        const val DAEMON_SOURCE_NAT66_UDP = "/nat66/udp"
        const val DAEMON_SOURCE_NAT66_ICMPV6 = "/nat66/icmpv6"
    }

    @androidx.room.Dao
    abstract class Dao {
        @Insert
        protected abstract fun insertInternal(value: TrafficRecord): Long
        fun insert(value: TrafficRecord) {
            check(value.id == null)
            value.id = insertInternal(value)
        }

        @Query("""
            SELECT  CASE
                        WHEN TrafficRecord.ip != :daemonSourceAddress THEN NULL
                        ELSE TrafficRecord.upstream
                    END AS marker,
                    MIN(TrafficRecord.timestamp) AS timestamp,
                    SUM(TrafficRecord.sentPackets) AS sentPackets,
                    SUM(TrafficRecord.sentBytes) AS sentBytes,
                    SUM(TrafficRecord.receivedPackets) AS receivedPackets,
                    SUM(TrafficRecord.receivedBytes) AS receivedBytes,
                    MAX(CASE
                        WHEN TrafficRecord.ip = :daemonSourceAddress AND
                            TrafficRecord.upstream = '/nat66/tcp' AND
                            TrafficRecord.sentPackets = 0 AND (
                                TrafficRecord.sentBytes != 0 OR TrafficRecord.receivedBytes != 0
                            ) THEN 1
                        ELSE 0
                    END) AS hasLegacyNat66TcpRows
                FROM TrafficRecord LEFT JOIN TrafficRecord AS Next ON TrafficRecord.id = Next.previousId
                /* We only want to find the last record for each chain so that we don't double count */
                WHERE TrafficRecord.mac = :mac AND Next.id IS NULL AND (
                    TrafficRecord.ip != :daemonSourceAddress OR
                    TrafficRecord.upstream IN ('/dns', '/nat66/tcp', '/nat66/udp', '/nat66/icmpv6')
                )
                GROUP BY CASE
                    WHEN TrafficRecord.ip != :daemonSourceAddress THEN NULL
                    ELSE TrafficRecord.upstream
                END
                HAVING SUM(TrafficRecord.sentPackets) != 0 OR SUM(TrafficRecord.sentBytes) != 0 OR
                    SUM(TrafficRecord.receivedPackets) != 0 OR SUM(TrafficRecord.receivedBytes) != 0
                """)
        protected abstract suspend fun queryStatsRows(
                mac: MacAddress,
                daemonSourceAddress: InetAddress
        ): List<ClientStatsRow>
        suspend fun queryStats(mac: MacAddress) = ClientStats(queryStatsRows(mac, DAEMON_SOURCE_ADDRESS).mapNotNull {
            it.toStats()
        }.sortedBy { it.source.ordinal })
    }
}

@Parcelize
data class ClientStats(val entries: List<ClientStatsEntry> = emptyList()) : Parcelable {
    val timestamp get() = entries.minOfOrNull { it.timestamp } ?: 0
}

@Parcelize
data class ClientStatsEntry(
        val source: TrafficStatsSource,
        val timestamp: Long = 0,
        val sentPackets: Long = 0,
        val sentBytes: Long = 0,
        val receivedPackets: Long = 0,
        val receivedBytes: Long = 0,
        val connectionCountKnown: Boolean = true
) : Parcelable {
    val isEmpty get() = sentPackets == 0L && sentBytes == 0L && receivedPackets == 0L && receivedBytes == 0L
}

enum class TrafficStatsSource {
    IPV4,
    DNS,
    NAT66_TCP,
    NAT66_UDP,
    NAT66_ICMPV6,
}

data class ClientStatsRow(
        val marker: String?,
        val timestamp: Long = 0,
        val sentPackets: Long = 0,
        val sentBytes: Long = 0,
        val receivedPackets: Long = 0,
        val receivedBytes: Long = 0,
        val hasLegacyNat66TcpRows: Boolean = false
) {
    fun toStats(): ClientStatsEntry? {
        val source = when (marker) {
            null -> TrafficStatsSource.IPV4
            TrafficRecord.DAEMON_SOURCE_DNS -> TrafficStatsSource.DNS
            TrafficRecord.DAEMON_SOURCE_NAT66_TCP -> TrafficStatsSource.NAT66_TCP
            TrafficRecord.DAEMON_SOURCE_NAT66_UDP -> TrafficStatsSource.NAT66_UDP
            TrafficRecord.DAEMON_SOURCE_NAT66_ICMPV6 -> TrafficStatsSource.NAT66_ICMPV6
            else -> return null
        }
        return ClientStatsEntry(
                source, timestamp, sentPackets, sentBytes, receivedPackets, receivedBytes,
                source != TrafficStatsSource.NAT66_TCP || !hasLegacyNat66TcpRows)
    }
}
