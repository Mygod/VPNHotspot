package be.mygod.vpnhotspot.room

import androidx.room.*

@Entity
data class ClientRecord(@PrimaryKey
                        val mac: Long,
                        var nickname: CharSequence = "",
                        var blocked: Boolean = false) {
    @androidx.room.Dao
    interface Dao {
        @Query("SELECT * FROM `ClientRecord` WHERE `mac` = :mac")
        fun lookupOrNull(mac: Long): ClientRecord?

        @Insert(onConflict = OnConflictStrategy.REPLACE)
        fun update(value: ClientRecord): Long
    }
}

fun ClientRecord.Dao.lookup(mac: Long) = lookupOrNull(mac) ?: ClientRecord(mac)
