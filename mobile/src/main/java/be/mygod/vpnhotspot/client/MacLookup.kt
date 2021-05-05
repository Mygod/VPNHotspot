package be.mygod.vpnhotspot.client

import android.content.Context
import android.os.Build
import androidx.annotation.MainThread
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.widget.SmartSnackbar
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
    class UnexpectedError(val mac: MacAddressCompat, val error: String) : JSONException("") {
        private fun formatMessage(context: Context) =
                context.getString(R.string.clients_mac_lookup_unexpected_error, mac.toOui(), error)
        override val message get() = formatMessage(app.english)
        override fun getLocalizedMessage() = formatMessage(app)
    }

    private val macLookupBusy = mutableMapOf<MacAddressCompat, Pair<HttpURLConnection, Job>>()

    @MainThread
    fun abort(mac: MacAddressCompat) = macLookupBusy.remove(mac)?.let { (conn, job) ->
        job.cancel()
        if (Build.VERSION.SDK_INT >= 26) conn.disconnect() else GlobalScope.launch(Dispatchers.IO) { conn.disconnect() }
    }

    @MainThread
    fun perform(mac: MacAddressCompat, explicit: Boolean = false) {
        abort(mac)
        val conn = URL("https://api.maclookup.app/v2/macs/${mac.toOui()}").openConnection() as HttpURLConnection
        macLookupBusy[mac] = conn to GlobalScope.launch(Dispatchers.IO) {
            try {
                val response = conn.inputStream.bufferedReader().readText()
                val obj = JSONObject(response)
                if (!obj.getBoolean("success")) throw UnexpectedError(mac, response)
                if (!obj.getBoolean("found")) {
                    // no vendor found, we should not retry in the future
                    AppDatabase.instance.clientRecordDao.upsert(mac) { macLookupPending = false }
                    return@launch
                }
                val country = obj.getString("country")
                val company = obj.getString("company")
                val result = if (country.length != 2) {
                    Timber.w(UnexpectedError(mac, response))
                    company
                } else String(country.flatMap { listOf('\uD83C', it + 0xDDA5) }.toCharArray()) + ' ' + company
                AppDatabase.instance.clientRecordDao.upsert(mac) {
                    nickname = result
                    macLookupPending = false
                }
            } catch (e: JSONException) {
                Timber.w(e)
                if (explicit) SmartSnackbar.make(e).show()
            } catch (e: IOException) {
                Timber.d(e)
                if (explicit) SmartSnackbar.make(e).show()
            }
        }
    }
}
