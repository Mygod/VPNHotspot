package be.mygod.vpnhotspot.room

import androidx.lifecycle.LiveData
import androidx.lifecycle.map
import androidx.room.*
import be.mygod.vpnhotspot.net.MacAddressCompat

@Entity
data class ClientRecord(@PrimaryKey
                        val mac: Long,
                        var nickname: CharSequence = "",
                        var blocked: Boolean = false,
                        var macLookupPending: Boolean = true) {
    @androidx.room.Dao
    abstract class Dao {
        @Query("SELECT * FROM `ClientRecord` WHERE `mac` = :mac")
        protected abstract fun lookupBlocking(mac: Long): ClientRecord?
        fun lookupOrDefaultBlocking(mac: MacAddressCompat) = lookupBlocking(mac.addr) ?: ClientRecord(mac.addr)

        @Query("SELECT * FROM `ClientRecord` WHERE `mac` = :mac")
        protected abstract suspend fun lookup(mac: Long): ClientRecord?
        suspend fun lookupOrDefault(mac: Long) = lookup(mac) ?: ClientRecord(mac)

        @Query("SELECT * FROM `ClientRecord` WHERE `mac` = :mac")
        protected abstract fun lookupSync(mac: Long): LiveData<ClientRecord?>
        fun lookupOrDefaultSync(mac: MacAddressCompat) = lookupSync(mac.addr).map { it ?: ClientRecord(mac.addr) }

        @Insert(onConflict = OnConflictStrategy.REPLACE)
        protected abstract suspend fun updateInternal(value: ClientRecord): Long
        suspend fun update(value: ClientRecord) = check(updateInternal(value) == value.mac)

        @Transaction
        open suspend fun upsert(mac: MacAddressCompat, operation: suspend ClientRecord.() -> Unit) = lookupOrDefault(
                mac.addr).apply {
            operation()
            update(this)
        }
    }
}
