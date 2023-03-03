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
import java.math.BigInteger
import java.security.MessageDigest

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

    private val sha1 = MessageDigest.getInstance("SHA-1")
    private val macLookupBusy = mutableMapOf<MacAddress, Job>()
    // http://en.wikipedia.org/wiki/ISO_3166-1
    private val countryCodeRegex = "(?:^|[^A-Z])([A-Z]{2})[\\s\\d]*$".toRegex()

    @MainThread
    fun abort(mac: MacAddress) = macLookupBusy.remove(mac)?.cancel()

    @MainThread
    fun perform(mac: MacAddress, explicit: Boolean = false) {
        abort(mac)
        macLookupBusy[mac] = GlobalScope.launch(Dispatchers.Unconfined, CoroutineStart.UNDISPATCHED) {
            var response: String? = null
            try {
                response = connectCancellable("https://api.maclookup.app/v2/macs/$mac") { conn ->
//                    conn.setRequestProperty("X-App-Id", "net.mobizme.macaddress")
//                    conn.setRequestProperty("X-App-Version", "2.0.11")
//                    conn.setRequestProperty("X-App-Version-Code", "111")
                    val epoch = System.currentTimeMillis()
                    conn.setRequestProperty("X-App-Epoch", epoch.toString())
                    conn.setRequestProperty("X-App-Sign", "%032x".format(BigInteger(1,
                        sha1.digest("aBA6AEkfg8cbHlWrBDYX_${mac}_$epoch".toByteArray()))))
                    when (val responseCode = conn.responseCode) {
                        200 -> conn.inputStream.bufferedReader().readText()
                        400, 401, 429 -> throw UnexpectedError(mac, conn.inputStream.bufferedReader().readText())
                        else -> throw UnexpectedError(mac, "Unhandled response code $responseCode: " +
                                conn.inputStream.bufferedReader().readText())
                    }
                }
                val obj = JSONObject(response)
                if (!obj.getBoolean("success")) throw UnexpectedError(mac, response)
                val result = if (obj.getBoolean("found")) {
                    val company = obj.getString("company")
                    val match = extractCountry(mac, response, obj)
                    if (match != null) {
                        String(match.groupValues[1].flatMap { listOf('\uD83C', it + 0xDDA5) }.toCharArray()) + ' ' +
                                company
                    } else company
                } else null
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

    private fun extractCountry(mac: MacAddress, response: String, obj: JSONObject): MatchResult? {
        countryCodeRegex.matchEntire(obj.optString("country"))?.also { return it }
        val address = obj.optString("address")
        if (address.isBlank()) return null
        countryCodeRegex.find(address)?.also { return it }
        Timber.w(UnexpectedError(mac, response))
        return null
    }
}
