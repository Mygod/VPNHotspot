package be.mygod.vpnhotspot.room

import android.os.Parcel
import android.os.Parcelable
import androidx.room.*
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

        @Query("""
            SELECT  MIN(TrafficRecord.timestamp) AS timestamp,
                    COUNT(TrafficRecord.id) AS count,
                    SUM(TrafficRecord.sentPackets) AS sentPackets,
                    SUM(TrafficRecord.sentBytes) AS sentBytes,
                    SUM(TrafficRecord.receivedPackets) AS receivedPackets,
                    SUM(TrafficRecord.receivedBytes) AS receivedBytes
                FROM TrafficRecord LEFT JOIN TrafficRecord AS Next ON TrafficRecord.id = Next.previousId
                /* We only want to find the last record for each chain so that we don't double count */
                WHERE TrafficRecord.mac = :mac AND Next.id IS NULL""")
        fun queryStats(mac: Long): ClientStats
    }
}

fun TrafficRecord.Dao.insert(value: TrafficRecord) {
    check(value.id == null)
    value.id = insertInternal(value)
}

data class ClientStats(
        val timestamp: Long = 0,
        val count: Long = 0,
        val sentPackets: Long = 0,
        val sentBytes: Long = 0,
        val receivedPackets: Long = 0,
        val receivedBytes: Long = 0
) : Parcelable {
    constructor(parcel: Parcel) : this(
            parcel.readLong(),
            parcel.readLong(),
            parcel.readLong(),
            parcel.readLong(),
            parcel.readLong(),
            parcel.readLong())

    override fun writeToParcel(parcel: Parcel, flags: Int) {
        parcel.writeLong(timestamp)
        parcel.writeLong(count)
        parcel.writeLong(sentPackets)
        parcel.writeLong(sentBytes)
        parcel.writeLong(receivedPackets)
        parcel.writeLong(receivedBytes)
    }

    override fun describeContents() = 0

    companion object CREATOR : Parcelable.Creator<ClientStats> {
        override fun createFromParcel(parcel: Parcel) = ClientStats(parcel)

        override fun newArray(size: Int) = arrayOfNulls<ClientStats>(size)
    }
}
