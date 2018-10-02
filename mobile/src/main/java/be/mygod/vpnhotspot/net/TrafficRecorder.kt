package be.mygod.vpnhotspot.net

import android.os.SystemClock
import android.util.Log
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.TrafficRecord
import be.mygod.vpnhotspot.room.insert
import be.mygod.vpnhotspot.room.macToLong
import be.mygod.vpnhotspot.util.Event2
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.util.parseNumericAddress
import com.crashlytics.android.Crashlytics
import java.net.InetAddress
import java.util.concurrent.TimeUnit

object TrafficRecorder {
    private const val TAG = "TrafficRecorder"
    private const val ANYWHERE = "0.0.0.0/0"

    private var scheduled = false
    private val records = HashMap<Pair<InetAddress, String>, TrafficRecord>()
    val foregroundListeners = Event2<Collection<TrafficRecord>, HashMap<Long, TrafficRecord>>()

    fun register(ip: InetAddress, upstream: String?, downstream: String, mac: String) {
        val record = TrafficRecord(
                mac = mac.macToLong(),
                ip = ip,
                upstream = upstream,
                downstream = downstream)
        AppDatabase.instance.trafficRecordDao.insert(record)
        synchronized(this) {
            check(records.put(Pair(ip, downstream), record) == null)
            scheduleUpdateLocked()
        }
    }
    fun unregister(ip: InetAddress, downstream: String) = synchronized(this) {
        update()    // flush stats before removing
        check(records.remove(Pair(ip, downstream)) != null)
    }

    private fun scheduleUpdateLocked() {
        if (scheduled) return
        val now = System.currentTimeMillis()
        val minute = TimeUnit.MINUTES.toMillis(1)
        var timeout = minute - now % minute
        if (foregroundListeners.isNotEmpty() && timeout > 1000) timeout = 1000
        RootSession.handler.postAtTime(this::update, this, SystemClock.uptimeMillis() + timeout)
        scheduled = true
    }

    fun update() {
        synchronized(this) {
            scheduled = false
            if (records.isEmpty()) return
            val timestamp = System.currentTimeMillis()
            val oldRecords = HashMap<Long, TrafficRecord>()
            for (line in RootSession.use { it.execOutUnjoined("iptables -nvx -L vpnhotspot_fwd") }
                    .asSequence().drop(2)) {
                val columns = line.split("\\s+".toRegex()).filter { it.isNotEmpty() }
                try {
                    check(columns.size >= 9)
                    when (columns[2]) {
                        "DROP" -> { }
                        "ACCEPT" -> {
                            val isReceive = columns[7] == ANYWHERE
                            val isSend = columns[8] == ANYWHERE
                            check(isReceive != isSend)
                            val ip = parseNumericAddress(columns[if (isReceive) 8 else 7])
                            val downstream = columns[if (isReceive) 6 else 5]
                            var upstream: String? = columns[if (isReceive) 5 else 6]
                            if (upstream == "*") upstream = null
                            val key = Pair(ip, downstream)
                            val oldRecord = records[key]!!
                            check(upstream == oldRecord.upstream)
                            val record = if (oldRecord.id == null) oldRecord else TrafficRecord(
                                    timestamp = timestamp,
                                    mac = oldRecord.mac,
                                    ip = ip,
                                    upstream = upstream,
                                    downstream = downstream,
                                    previousId = oldRecord.id)
                            if (isReceive) {
                                record.receivedPackets = columns[0].toLong()
                                record.receivedBytes = columns[1].toLong()
                            } else {
                                record.sentPackets = columns[0].toLong()
                                record.sentBytes = columns[1].toLong()
                            }
                            if (oldRecord.id != null) {
                                check(records.put(key, record) == oldRecord)
                                check(oldRecords.put(oldRecord.id!!, oldRecord) == null)
                            }
                        }
                        else -> check(false)
                    }
                } catch (e: RuntimeException) {
                    Crashlytics.log(Log.WARN, TAG, line)
                    e.printStackTrace()
                    Crashlytics.logException(e)
                }
            }
            for ((_, record) in records) if (record.id == null) AppDatabase.instance.trafficRecordDao.insert(record)
            foregroundListeners(records.values, oldRecords)
            scheduleUpdateLocked()
        }
    }

    fun clean() = synchronized(this) {
        update()
        records.clear()
    }
}
