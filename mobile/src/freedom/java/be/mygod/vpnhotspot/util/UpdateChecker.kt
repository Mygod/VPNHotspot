package be.mygod.vpnhotspot.util

import android.app.Activity
import android.net.Uri
import androidx.core.content.edit
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.BuildConfig
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.cancellable
import kotlinx.coroutines.flow.flow
import org.json.JSONArray
import timber.log.Timber
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.time.Instant
import java.util.concurrent.CancellationException
import java.util.concurrent.TimeUnit
import kotlin.math.max
import kotlin.math.min

object UpdateChecker {
    private const val KEY_LAST_FETCHED = "update.lastFetched"
    private const val KEY_VERSION = "update.version"
    private const val KEY_PUBLISHED = "update.published"
    private const val UPDATE_INTERVAL = 1000 * 60 * 60 * 6

    private data class GitHubUpdate(override val message: String, val published: Long) : AppUpdate {
        override val stalenessDays get() = max(0,
            TimeUnit.DAYS.convert(System.currentTimeMillis() - published, TimeUnit.MILLISECONDS)).toInt()

        override fun updateForResult(activity: Activity, requestCode: Int) {
            app.customTabsIntent.launchUrl(activity, Uri.parse("https://github.com/Mygod/VPNHotspot/releases"))
        }
    }

    private data class SemVer(val major: Int, val minor: Int, val revision: Int) : Comparable<SemVer> {
        override fun compareTo(other: SemVer): Int {
            var result = major - other.major
            if (result != 0) return result
            result = minor - other.minor
            if (result != 0) return result
            return revision - other.revision
        }
    }
    private val semverParser = "^v?(\\d+)\\.(\\d+)\\.(\\d+)(?:-|$)".toPattern()
    private fun CharSequence.toSemVer() = semverParser.matcher(this).let { matcher ->
        require(matcher.find()) { "Unrecognized version $this" }
        SemVer(matcher.group(1)!!.toInt(), matcher.group(2)!!.toInt(), matcher.group(3)!!.toInt())
    }
    private val myVer = BuildConfig.VERSION_NAME.toSemVer()

    private fun findUpdate(response: JSONArray): GitHubUpdate? {
        var latest: String? = null
        var latestVer = myVer
        var earliest = Long.MAX_VALUE
        for (i in 0 until response.length()) {
            val obj = response.getJSONObject(i)
            val name = obj.getString("name")
            val semver = try {
                name.toSemVer()
            } catch (e: IllegalArgumentException) {
                Timber.w(e)
                continue
            }
            if (semver <= myVer) continue
            if (semver > latestVer) {
                latest = name
                latestVer = semver
            }
            earliest = min(earliest, Instant.parse(obj.getString("published_at")).toEpochMilli())
        }
        return latest?.let { GitHubUpdate(it, earliest) }
    }
    fun check() = flow<AppUpdate?> {
        emit(app.pref.getString(KEY_VERSION, null)?.let {
            if (myVer >= it.toSemVer()) null else GitHubUpdate(it, app.pref.getLong(KEY_PUBLISHED, -1))
        })
        while (true) {
            val now = System.currentTimeMillis()
            val lastFetched = app.pref.getLong(KEY_LAST_FETCHED, -1)
            if (lastFetched in 0..now) delay(lastFetched + UPDATE_INTERVAL - now)
            currentCoroutineContext().ensureActive()
            val conn = URL("https://api.github.com/repos/Mygod/VPNHotspot/releases?per_page=100")
                .openConnection() as HttpURLConnection
            var reset: Long? = null
            app.pref.edit {
                try {
                    conn.setRequestProperty("Accept", "application/vnd.github.v3+json")
                    val update = findUpdate(JSONArray(withContext(Dispatchers.IO) {
                        reset = conn.getHeaderField("X-RateLimit-Reset")?.toLongOrNull()
                        conn.inputStream.bufferedReader().readText()
                    }))
                    putString(KEY_VERSION, update?.let {
                        putLong(KEY_PUBLISHED, update.published)
                        it.message
                    })
                    emit(update)
                } catch (_: CancellationException) {
                    return@flow
                } catch (e: IOException) {
                    Timber.d(e)
                } catch (e: Exception) {
                    Timber.w(e)
                } finally {
                    conn.disconnectCompat()
                    putLong(KEY_LAST_FETCHED, System.currentTimeMillis())
                }
            }
            reset?.let { delay(System.currentTimeMillis() - it * 1000) }
        }
    }.cancellable()
}
