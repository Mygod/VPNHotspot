package be.mygod.vpnhotspot.client

import android.content.Context
import android.net.MacAddress
import androidx.annotation.MainThread
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.util.connectCancellable
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import org.json.JSONException
import org.json.JSONObject
import timber.log.Timber
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException
import java.net.HttpCookie
import java.util.Scanner
import java.util.regex.Pattern

/**
 * This class generates a default nickname for new clients.
 */
object MacLookup {
    class UnexpectedError(val mac: MacAddress, val error: String) : JSONException("") {
        private fun formatMessage(context: Context) =
            context.getString(R.string.clients_mac_lookup_unexpected_error,
                mac.toByteArray().joinToString("") { "%02x".format(it) }.substring(0, 9), error)
        override val message get() = formatMessage(app.english)
        override fun getLocalizedMessage() = formatMessage(app)
    }

    private object SessionManager {
        private const val CACHE_FILENAME = "maclookup_sessioncache"
        private const val COOKIE_SESSION = "mac_address_vendor_lookup_session"
        private val csrfPattern = Pattern.compile("<meta\\s+name=\"csrf-token\"\\s+content=\"([^\"]*)\"",
            Pattern.CASE_INSENSITIVE)
        private var sessionCache: List<String>?
            get() = try {
                File(app.deviceStorage.cacheDir, CACHE_FILENAME).readText().split('\n', limit = 2)
            } catch (_: FileNotFoundException) {
                null
            }
            set(value) = File(app.deviceStorage.cacheDir, CACHE_FILENAME).run {
                if (value != null) writeText(value.joinToString("\n")) else if (!delete()) writeText("")
            }
        private val mutex = Mutex()

        private suspend fun refreshSessionCache() = connectCancellable("https://macaddress.io/api") { conn ->
            val cookies = conn.headerFields["set-cookie"] ?: throw IOException("Missing cookies")
            var mavls: HttpCookie? = null
            for (header in cookies) for (cookie in HttpCookie.parse(header)) {
                if (cookie.name == COOKIE_SESSION) mavls = cookie
            }
            if (mavls == null) throw IOException("Missing set-cookie $COOKIE_SESSION")
            val token = conn.inputStream.use { Scanner(it).findWithinHorizon(csrfPattern, 0) }
                ?: throw IOException("Missing csrf-token")
            listOf(mavls.toString(), csrfPattern.matcher(token).run {
                check(matches())
                group(1)!!
            }).also { sessionCache = it }
        }

        suspend fun obtain(forceNew: Boolean): Pair<HttpCookie, String> = mutex.withLock {
            val sessionCache = (if (forceNew) null else sessionCache) ?: refreshSessionCache()
            HttpCookie.parse(sessionCache[0]).single() to sessionCache[1]
        }
    }

    private val macLookupBusy = mutableMapOf<MacAddress, Job>()
    // http://en.wikipedia.org/wiki/ISO_3166-1
    private val countryCodeRegex = "(?:^|[^A-Z])([A-Z]{2})[\\s\\d]*$".toRegex()

    @MainThread
    fun abort(mac: MacAddress) = macLookupBusy.remove(mac)?.cancel()

    @MainThread
    fun perform(mac: MacAddress, explicit: Boolean = false) {
        abort(mac)
        macLookupBusy[mac] = GlobalScope.launch(Dispatchers.IO) {
            try {
                var response: String? = null
                for (tries in 0 until 5) {
                    val (cookie, csrf) = SessionManager.obtain(tries > 0)
                    response = connectCancellable("https://macaddress.io/mac-address-lookup") { conn ->
                        conn.requestMethod = "POST"
                        conn.setRequestProperty("content-type", "application/json")
                        conn.setRequestProperty("cookie", "${cookie.name}=${cookie.value}")
                        conn.setRequestProperty("x-csrf-token", csrf)
                        conn.outputStream.writer().use { it.write("{\"macAddress\":\"$mac\",\"not-web-search\":true}") }
                        when (val responseCode = conn.responseCode) {
                            200 -> conn.inputStream.bufferedReader().readText()
                            419 -> null
                            else -> throw IOException("Unhandled response code $responseCode")
                        }
                    }
                    if (response != null) break
                }
                if (response == null) throw IOException("Session creation failure")
                val obj = JSONObject(response)
                val result = if (obj.getJSONObject("blockDetails").getBoolean("blockFound")) {
                    val vendor = obj.getJSONObject("vendorDetails")
                    val company = vendor.getString("companyName")
                    val match = extractCountry(mac, response, vendor)
                    if (match != null) {
                        String(match.groupValues[1].flatMap { listOf('\uD83C', it + 0xDDA5) }.toCharArray()) + ' ' +
                                company
                    } else company
                } else null
                AppDatabase.instance.clientRecordDao.upsert(mac) {
                    if (result != null) nickname = result
                    macLookupPending = false
                }
            } catch (e: Throwable) {
                Timber.w(e)
                if (explicit) SmartSnackbar.make(e).show()
            }
        }
    }

    private fun extractCountry(mac: MacAddress, response: String, obj: JSONObject): MatchResult? {
        countryCodeRegex.matchEntire(obj.optString("countryCode"))?.also { return it }
        val address = obj.optString("companyAddress")
        if (address.isBlank()) return null
        countryCodeRegex.find(address)?.also { return it }
        Timber.w(UnexpectedError(mac, response))
        return null
    }
}
