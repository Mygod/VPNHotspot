package be.mygod.vpnhotspot.room

import androidx.room.Database
import androidx.room.Room
import androidx.room.RoomDatabase
import androidx.room.TypeConverters
import androidx.room.migration.Migration
import androidx.sqlite.db.SupportSQLiteDatabase
import be.mygod.vpnhotspot.App.Companion.app
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch

@Database(entities = [ClientRecord::class, TrafficRecord::class], version = 2)
@TypeConverters(Converters::class)
abstract class AppDatabase : RoomDatabase() {
    companion object {
        const val DB_NAME = "app.db"

        val instance by lazy {
            Room.databaseBuilder(app.deviceStorage, AppDatabase::class.java, DB_NAME).apply {
                addMigrations(
                        Migration2
                )
                setQueryExecutor { GlobalScope.launch { it.run() } }
            }.build()
        }
    }

    abstract val clientRecordDao: ClientRecord.Dao
    abstract val trafficRecordDao: TrafficRecord.Dao

    object Migration2 : Migration(1, 2) {
        override fun migrate(database: SupportSQLiteDatabase) =
            database.execSQL("ALTER TABLE `ClientRecord` ADD COLUMN `macLookupPending` INTEGER NOT NULL DEFAULT 1")
    }
}
