package be.mygod.vpnhotspot.room

import androidx.room.*

@Entity
data class ClientRecord(@PrimaryKey
                        val mac: Long,
                        var nickname: CharSequence = "",
                        var access: AccessRule = AccessRule.ALLOWED) {
    enum class AccessRule {
        /**
         * Client is allowed to access VPN and fallback network in non-strict mode.
         */
        ALLOWED,
        /**
         * Client is only allowed to access fallback network in non-strict mode.
         */
        BYPASSED,
        /**
         * Client is not allowed to access any network.
         */
        BLOCKED,
        ;
    }

    @androidx.room.Dao
    interface Dao {
        @Query("SELECT * FROM `ClientRecord` WHERE `mac` = :mac")
        fun lookupOrNull(mac: Long): ClientRecord?

        @Insert(onConflict = OnConflictStrategy.REPLACE)
        fun update(value: ClientRecord): Long
    }
}

fun ClientRecord.Dao.lookup(mac: Long) = lookupOrNull(mac) ?: ClientRecord(mac)
