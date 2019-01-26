package be.mygod.vpnhotspot.room

import androidx.room.testing.MigrationTestHelper
import androidx.sqlite.db.framework.FrameworkSQLiteOpenHelperFactory
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import java.io.IOException

@RunWith(AndroidJUnit4::class)
class MigrationTest {
    companion object {
        private const val TEST_DB = "migration-test"
    }

    @get:Rule
    val privateDatabase = MigrationTestHelper(InstrumentationRegistry.getInstrumentation(),
            AppDatabase::class.java.canonicalName, FrameworkSQLiteOpenHelperFactory())

    @Test
    @Throws(IOException::class)
    fun migrate2() {
        val db = privateDatabase.createDatabase(TEST_DB, 1)
        db.close()
        privateDatabase.runMigrationsAndValidate(TEST_DB, 2, true, AppDatabase.Migration2)
    }
}
