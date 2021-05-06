package be.mygod.vpnhotspot.client

import android.content.Context
import android.os.Build
import androidx.annotation.MainThread
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.*
import org.json.JSONException
import org.json.JSONObject
import timber.log.Timber
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import kotlin.math.max

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

    private data class Work(@Volatile var conn: HttpURLConnection, var job: Job? = null)
    private val macLookupBusy = mutableMapOf<MacAddressCompat, Work>()

    @MainThread
    fun abort(mac: MacAddressCompat) = macLookupBusy.remove(mac)?.let { work ->
        work.job!!.cancel()
        if (Build.VERSION.SDK_INT < 26) GlobalScope.launch(Dispatchers.IO) {
            work.conn.disconnect()
        } else work.conn.disconnect()
    }

    @MainThread
    fun perform(mac: MacAddressCompat, explicit: Boolean = false) {
        abort(mac)
        val url = URL("https://api.maclookup.app/v2/macs/${mac.toOui()}")
        val work = Work(url.openConnection() as HttpURLConnection)
        work.job = GlobalScope.launch(Dispatchers.IO) {
            try {
                while (work.conn.responseCode != 200) {
                    if (work.conn.responseCode != 429) {
                        throw UnexpectedError(mac, work.conn.inputStream.bufferedReader().readText())
                    }
                    work.conn = url.openConnection() as HttpURLConnection
                    delay(max(1, work.conn.getHeaderField("Retry-After")?.toLongOrNull().let {
                        if (it == null) {
                            Timber.w(UnexpectedError(mac,
                                work.conn.headerFields.entries.joinToString { (k, v) -> "$k: $v" }))
                            1
                        } else it
                    }) * 1000)
                }
                val response = work.conn.inputStream.bufferedReader().readText()
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
        macLookupBusy[mac] = work
    }
}
