package be.mygod.vpnhotspot.room

import androidx.room.Database
import androidx.room.Room
import androidx.room.RoomDatabase
import androidx.room.TypeConverters
import be.mygod.vpnhotspot.App.Companion.app

@Database(entities = [ClientRecord::class, TrafficRecord::class], version = 1)
@TypeConverters(Converters::class)
abstract class AppDatabase : RoomDatabase() {
    companion object {
        const val DB_NAME = "app.db"

        val instance by lazy {
            Room.databaseBuilder(app.deviceStorage, AppDatabase::class.java, DB_NAME)
                    .allowMainThreadQueries()
                    .build()
        }
    }

    abstract val clientRecordDao: ClientRecord.Dao
    abstract val trafficRecordDao: TrafficRecord.Dao
}
