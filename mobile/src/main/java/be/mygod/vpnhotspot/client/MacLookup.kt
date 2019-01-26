package be.mygod.vpnhotspot.client

import androidx.annotation.MainThread
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.macToString
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import org.json.JSONObject
import timber.log.Timber
import java.net.HttpURLConnection
import java.net.URL

/**
 * This class generates a default nickname for new clients.
 */
object MacLookup {
    private val macLookupBusy = mutableMapOf<Long, Pair<HttpURLConnection, Job>>()

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
                val obj = JSONObject(conn.inputStream.bufferedReader().readText()).getJSONObject("result")
                val error = obj.optString("error", null)
                if (error != null) {
                    Timber.i("Server returned error for ${mac.macToString()}: $error")
                    if (error == "no result") { // no vendor found, we should not retry in the future
                        AppDatabase.instance.clientRecordDao.upsert(mac) { macLookupPending = false }
                    }
                    return@launch
                }
                val company = obj.getString("company")
                val country = obj.optString("country")
                val result = if (country != null) {
                    String(country.flatMap { listOf('\uD83C', it + 0xDDA5) }.toCharArray()) + ' ' + company
                } else company
                AppDatabase.instance.clientRecordDao.upsert(mac) {
                    nickname = result
                    macLookupPending = false
                }
            } catch (e: Exception) {
                Timber.d(e)
            }
        }
    }
}
