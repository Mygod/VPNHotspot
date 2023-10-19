package be.mygod.vpnhotspot.client

import android.content.Context
import android.net.MacAddress
import androidx.annotation.MainThread
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.util.connectCancellable
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import org.json.JSONException
import org.json.JSONObject
import timber.log.Timber
import java.io.IOException
import java.net.HttpURLConnection
import java.util.Scanner
import java.util.regex.Pattern

/**
 * This class generates a default nickname for new clients.
 */
object MacLookup {
    class UnexpectedError(val mac: MacAddress, val error: String) : JSONException("") {
        private fun formatMessage(context: Context) =
            context.getString(R.string.clients_mac_lookup_unexpected_error,
                @OptIn(ExperimentalStdlibApi::class)
                mac.toByteArray().toHexString(0, 5).substring(0, 9), error)
        override val message get() = formatMessage(app.english)
        override fun getLocalizedMessage() = formatMessage(app)
    }

//    private val sha1 = MessageDigest.getInstance("SHA-1")
    private val macLookupBusy = mutableMapOf<MacAddress, Job>()
    // http://en.wikipedia.org/wiki/ISO_3166-1
    private val countryCodeRegex = "(?:^|[^A-Z])([A-Z]{2})[\\s\\d]*$".toRegex()
    // nanoid matcher with preceding pattern
    private val buildIdPattern by lazy { Pattern.compile("(?<=_next/static/|\"buildId\":\")[A-Za-z0-9_-]{21}") }

    private val HttpURLConnection.findErrorStream get() = errorStream ?: inputStream

    @MainThread
    fun abort(mac: MacAddress) = macLookupBusy.remove(mac)?.cancel()

    private var buildId = "er_PDXcqFFbzN-PV9pGR_"
    private suspend fun readResponse(mac: MacAddress, reportId: String): String {
        repeat(5) {
            connectCancellable(
                "https://mac-address.alldatafeeds.com/_next/data/$buildId/mac-address-lookup/$reportId.json") { conn ->
                when (val responseCode = conn.responseCode) {
                    200 -> conn.inputStream.bufferedReader().readText()
                    404, 500 -> {
                        buildId = conn.errorStream.use { Scanner(it).findWithinHorizon(buildIdPattern, 0) }
                            ?: throw UnexpectedError(mac, "failed to locate buildId in 404")
                        Timber.d("Obtained new buildId: $buildId")
                        null
                    }
                    else -> throw UnexpectedError(mac, "Unhandled response code $responseCode: " +
                            conn.findErrorStream.bufferedReader().readText())
                }
            }?.let { return it }
        }
        throw UnexpectedError(mac, "Repeated 404")
    }
    @MainThread
    fun perform(mac: MacAddress, explicit: Boolean = false) {
        abort(mac)
        macLookupBusy[mac] = GlobalScope.launch(Dispatchers.Unconfined, CoroutineStart.UNDISPATCHED) {
            var response: String? = null
            try {
                response = connectCancellable("https://mac-address.alldatafeeds.com/api/mac-address/lookup") { conn ->
                    conn.doOutput = true
                    conn.requestMethod = "POST"
                    conn.setRequestProperty("Content-Type", "application/json")
                    conn.outputStream.writer().use { it.write("{\"mac-address\":\"$mac\"}") }
                    when (val responseCode = conn.responseCode) {
                        200 -> conn.inputStream.bufferedReader().readText()
                        400, 401, 402, 404, 422, 429, 500 -> throw UnexpectedError(mac,
                            conn.findErrorStream.bufferedReader().readText())
                        else -> throw UnexpectedError(mac, "Unhandled response code $responseCode: " +
                                conn.findErrorStream.bufferedReader().readText())
                    }
                }
                response = readResponse(mac, JSONObject(response).getString("report_id"))
                val obj = JSONObject(response).getJSONObject("pageProps").getJSONObject("lookupResults")
                val result = if (obj.getJSONObject("blockDetails").getBoolean("blockFound")) {
                    val vendor = obj.getJSONObject("vendorDetails")
                    val company = vendor.getString("companyName")
                    val match = extractCountry(mac, response, vendor, obj)
                    if (match != null) {
                        String(match.groupValues[1].flatMap { listOf('\uD83C', it + 0xDDA5) }.toCharArray()) + ' ' +
                                company
                    } else company
                } else null
                Timber.d("$mac -> $result")
                AppDatabase.instance.clientRecordDao.upsert(mac) {
                    if (result != null) nickname = result
                    macLookupPending = false
                }
            } catch (_: CancellationException) {
            } catch (e: Throwable) {
                when (e) {
                    is UnexpectedError -> Timber.w(e)
                    is IOException -> Timber.d(e)
                    else -> Timber.w(UnexpectedError(mac, "Got response: $response").initCause(e))
                }
                if (explicit) SmartSnackbar.make(e).show()
            }
        }
    }

    private fun extractCountry(mac: MacAddress, response: String, vendor: JSONObject, obj: JSONObject): MatchResult? {
        for (candidate in sequence {
            yield(vendor)
            val history = obj.optJSONArray("blockHistory") ?: return@sequence
            for (i in history.length() - 1 downTo 0) {
                yield(history.getJSONObject(i))
            }
        }) countryCodeRegex.matchEntire(candidate.getString("countryCode"))?.also { return it }
        if (!vendor.getBoolean("isPrivate")) Timber.w(UnexpectedError(mac, response))
        return null
    }
}
