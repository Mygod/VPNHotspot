package be.mygod.vpnhotspot.client

import androidx.annotation.MainThread
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.macToString
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import org.json.JSONException
import org.json.JSONObject
import timber.log.Timber
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

/**
 * This class generates a default nickname for new clients.
 */
object MacLookup {
    class UnexpectedError(mac: Long, val error: String) :
            JSONException("Server returned error for ${mac.macToString()}: $error")

    private val macLookupBusy = mutableMapOf<Long, Pair<HttpURLConnection, Job>>()
    private val countryCodeRegex = "[A-Z]{2}".toRegex() // http://en.wikipedia.org/wiki/ISO_3166-1

    @MainThread
    fun abort(mac: Long) = macLookupBusy.remove(mac)?.let { (conn, job) ->
        job.cancel()
        conn.disconnect()
    }

    @MainThread
    fun perform(mac: Long) {
        abort(mac)
        val conn = URL("https://macvendors.co/api/" + mac.macToString()).openConnection() as HttpURLConnection
        macLookupBusy[mac] = conn to GlobalScope.launch(Dispatchers.IO) {
            try {
                val response = conn.inputStream.bufferedReader().readText()
                val obj = JSONObject(response).getJSONObject("result")
                obj.optString("error", null)?.also { throw UnexpectedError(mac, it) }
                val company = obj.getString("company")
                val country = obj.optString("country")
                val result = if (countryCodeRegex.matchEntire(country) != null) {
                    String(country.flatMap { listOf('\uD83C', it + 0xDDA5) }.toCharArray()) + ' ' + company
                } else company.also { Timber.w(UnexpectedError(mac, response)) }
                AppDatabase.instance.clientRecordDao.upsert(mac) {
                    nickname = result
                    macLookupPending = false
                }
            } catch (e: IOException) {
                Timber.d(e)
            } catch (e: JSONException) {
                if ((e as? UnexpectedError)?.error == "no result") {
                    // no vendor found, we should not retry in the future
                    AppDatabase.instance.clientRecordDao.upsert(mac) { macLookupPending = false }
                } else Timber.w(e)
            }
        }
    }
}
