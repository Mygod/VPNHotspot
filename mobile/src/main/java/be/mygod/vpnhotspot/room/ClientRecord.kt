package be.mygod.vpnhotspot.room

import androidx.lifecycle.LiveData
import androidx.room.*
import kotlinx.coroutines.runBlocking

@Entity
data class ClientRecord(@PrimaryKey
                        val mac: Long,
                        var nickname: CharSequence = "",
                        var blocked: Boolean = false,
                        var macLookupPending: Boolean = true) {
    @androidx.room.Dao
    abstract class Dao {
        @Query("SELECT * FROM `ClientRecord` WHERE `mac` = :mac")
        abstract suspend fun lookup(mac: Long): ClientRecord?

        suspend fun lookupOrDefault(mac: Long) = lookup(mac) ?: ClientRecord(mac)

        @Query("SELECT * FROM `ClientRecord` WHERE `mac` = :mac")
        abstract fun lookupSync(mac: Long): LiveData<ClientRecord>

        @Insert(onConflict = OnConflictStrategy.REPLACE)
        protected abstract suspend fun updateInternal(value: ClientRecord): Long
        suspend fun update(value: ClientRecord) = check(updateInternal(value) == value.mac)

        @Transaction
        open suspend fun upsert(mac: Long, operation: suspend ClientRecord.() -> Unit) = lookupOrDefault(mac).apply {
            operation()
            update(this)
        }
    }
}
