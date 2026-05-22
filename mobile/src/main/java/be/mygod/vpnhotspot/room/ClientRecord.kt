package be.mygod.vpnhotspot.room

import android.net.MacAddress
import androidx.compose.ui.text.AnnotatedString
import androidx.room.*
import be.mygod.vpnhotspot.net.MacAddressCompat.Companion.toLong
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.map

@Entity
data class ClientRecord(@PrimaryKey
                        val mac: MacAddress,
                        var nickname: AnnotatedString = AnnotatedString(""),
                        var blocked: Boolean = false,
                        var macLookupPending: Boolean = true) {
    @androidx.room.Dao
    abstract class Dao {
        @Query("SELECT * FROM `ClientRecord` WHERE `mac` = :mac")
        protected abstract fun lookupBlocking(mac: MacAddress): ClientRecord?
        fun lookupOrDefaultBlocking(mac: MacAddress) = lookupBlocking(mac) ?: ClientRecord(mac)

        @Query("SELECT * FROM `ClientRecord` WHERE `mac` = :mac")
        protected abstract suspend fun lookup(mac: MacAddress): ClientRecord?
        suspend fun lookupOrDefault(mac: MacAddress) = lookup(mac) ?: ClientRecord(mac)

        @Query("SELECT * FROM `ClientRecord` WHERE `mac` = :mac")
        protected abstract fun lookupFlow(mac: MacAddress): Flow<ClientRecord?>
        fun lookupOrDefaultFlow(mac: MacAddress) = lookupFlow(mac).map { it ?: ClientRecord(mac) }

        @Query("SELECT `mac` FROM `ClientRecord` WHERE `blocked`")
        abstract fun observeBlockedMacs(): Flow<List<MacAddress>>

        @Insert(onConflict = OnConflictStrategy.REPLACE)
        protected abstract suspend fun updateInternal(value: ClientRecord): Long
        suspend fun update(value: ClientRecord) = check(updateInternal(value) == value.mac.toLong())

        @Transaction
        open suspend fun upsert(mac: MacAddress, operation: suspend ClientRecord.() -> Unit) = lookupOrDefault(
            mac).apply {
            operation()
            update(this)
        }
    }
}
