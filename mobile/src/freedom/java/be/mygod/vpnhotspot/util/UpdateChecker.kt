package be.mygod.vpnhotspot.util

import android.app.Activity
import android.net.Uri
import androidx.core.content.edit
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.BuildConfig
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.withContext
import org.json.JSONObject
import timber.log.Timber
import java.net.HttpURLConnection
import java.net.URL
import java.time.Instant
import java.util.concurrent.CancellationException
import java.util.concurrent.TimeUnit
import kotlin.math.max

object UpdateChecker {
    private const val KEY_LAST_FETCHED = "update.lastFetched"
    private const val KEY_VERSION = "update.version"
    private const val KEY_PUBLISHED = "update.published"
    private const val UPDATE_INTERVAL = 1000 * 60 * 60 * 6

    private class GitHubUpdate(override val message: String, private val published: Long) : AppUpdate {
        override val stalenessDays get() = max(0,
            TimeUnit.DAYS.convert(System.currentTimeMillis() - published, TimeUnit.MILLISECONDS)).toInt()

        override fun updateForResult(activity: Activity, requestCode: Int) {
            app.customTabsIntent.launchUrl(activity, Uri.parse("https://github.com/Mygod/VPNHotspot/releases"))
        }
    }

    fun check() = flow<AppUpdate?> {
        val myVersion = "v${BuildConfig.VERSION_NAME}"
        emit(app.pref.getString(KEY_VERSION, null)?.let {
            if (myVersion == it) null else GitHubUpdate(it, app.pref.getLong(KEY_PUBLISHED, -1))
        })
        while (true) {
            val now = System.currentTimeMillis()
            val lastFetched = app.pref.getLong(KEY_LAST_FETCHED, -1)
            if (lastFetched in 0..now) delay(lastFetched + UPDATE_INTERVAL - now)
            val conn = URL("https://api.github.com/repos/Mygod/VPNHotspot/releases/latest")
                .openConnection() as HttpURLConnection
            try {
                conn.setRequestProperty("Accept", "application/vnd.github.v3+json")
                val response = JSONObject(withContext(Dispatchers.IO) {
                    conn.inputStream.bufferedReader().readText()
                })
                val version = response.getString("tag_name")
                val published = Instant.parse(response.getString("published_at")).toEpochMilli()
                app.pref.edit {
                    putLong(KEY_LAST_FETCHED, System.currentTimeMillis())
                    putString(KEY_VERSION, version)
                    putLong(KEY_PUBLISHED, published)
                }
                emit(if (myVersion == version) null else GitHubUpdate(version, published))
            } catch (_: CancellationException) {
                return@flow
            } catch (e: Exception) {
                Timber.w(e)
            } finally {
                conn.disconnectCompat()
            }
        }
    }
}
