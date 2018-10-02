package be.mygod.vpnhotspot.room

import androidx.room.*
import java.net.InetAddress

@Entity(foreignKeys = [ForeignKey(entity = TrafficRecord::class, parentColumns = ["id"], childColumns = ["previousId"],
            onDelete = ForeignKey.CASCADE, onUpdate = ForeignKey.RESTRICT)],
        indices = [Index(value = ["previousId"], unique = true)])
data class TrafficRecord(
        /**
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
        val mac: Long,
        /**
         * For now only stats for IPv4 will be recorded. But I'm going to put the more general class here just in case.
         */
        val ip: InetAddress,
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
    @androidx.room.Dao
    interface Dao {
        @Insert
        fun insertInternal(value: TrafficRecord): Long
    }
}

fun TrafficRecord.Dao.insert(value: TrafficRecord) {
    value.id = insertInternal(value)
}
