package be.mygod.vpnhotspot.net.monitor

import android.util.LongSparseArray
import androidx.core.os.postDelayed
import be.mygod.vpnhotspot.DebugHelper
import be.mygod.vpnhotspot.net.Routing.Companion.IPTABLES
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.TrafficRecord
import be.mygod.vpnhotspot.util.Event2
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.util.parseNumericAddress
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.net.InetAddress
import java.util.concurrent.TimeUnit

object TrafficRecorder {
    private const val TAG = "TrafficRecorder"
    private const val ANYWHERE = "0.0.0.0/0"

    private var scheduled = false
    private var lastUpdate = 0L
    private val records = HashMap<Pair<InetAddress, String>, TrafficRecord>()
    val foregroundListeners = Event2<Collection<TrafficRecord>, LongSparseArray<TrafficRecord>>()

    fun register(ip: InetAddress, downstream: String, mac: Long) {
        val record = TrafficRecord(mac = mac, ip = ip, downstream = downstream)
        AppDatabase.instance.trafficRecordDao.insert(record)
        synchronized(this) {
            DebugHelper.log(TAG, "Registering $ip%$downstream")
            check(records.put(Pair(ip, downstream), record) == null)
            scheduleUpdateLocked()
        }
    }
    fun unregister(ip: InetAddress, downstream: String) = synchronized(this) {
        update()    // flush stats before removing
        DebugHelper.log(TAG, "Unregistering $ip%$downstream")
        if (records.remove(Pair(ip, downstream)) == null) Timber.w("Failed to find traffic record for $ip%$downstream.")
    }

    private fun unscheduleUpdateLocked() {
        RootSession.handler.removeCallbacksAndMessages(this)
        scheduled = false
    }
    private fun scheduleUpdateLocked() {
        if (scheduled) return
        val now = System.currentTimeMillis()
        val minute = TimeUnit.MINUTES.toMillis(1)
        var timeout = minute - now % minute
        if (foregroundListeners.isNotEmpty() && timeout > 1000) timeout = 1000
        RootSession.handler.postDelayed(timeout, this) { update(true) }
        scheduled = true
    }

    fun rescheduleUpdate() = synchronized(this) {
        unscheduleUpdateLocked()
        scheduleUpdateLocked()
    }

    private fun doUpdate(timestamp: Long) {
        val oldRecords = LongSparseArray<TrafficRecord>()
        loop@ for (line in RootSession.use {
            val command = "$IPTABLES -nvx -L vpnhotspot_fwd"
            val result = it.execQuiet(command)
            val message = RootSession.checkOutput(command, result, false, false)
            if (result.err.isNotEmpty()) Timber.i(message)
            result.out.drop(2)
        }) {
            val columns = line.split("\\s+".toRegex()).filter { it.isNotEmpty() }
            try {
                check(columns.size >= 9)
                when (columns[2]) {
                    "DROP" -> { }
                    "ACCEPT" -> {
                        val isReceive = columns[7] == ANYWHERE
                        val isSend = columns[8] == ANYWHERE
                        check(isReceive != isSend)  // this check might fail when the user performed an upgrade from 1.x
                        val ip = parseNumericAddress(columns[if (isReceive) 8 else 7])
                        val downstream = columns[if (isReceive) 6 else 5]
                        val key = Pair(ip, downstream)
                        val oldRecord = records[key] ?: continue@loop   // assuming they're legacy old rules
                        val record = if (oldRecord.id == null) oldRecord else TrafficRecord(
                                timestamp = timestamp,
                                mac = oldRecord.mac,
                                ip = ip,
                                downstream = downstream,
                                sentPackets = -1,
                                sentBytes = -1,
                                receivedPackets = -1,
                                receivedBytes = -1,
                                previousId = oldRecord.id)
                        if (isReceive) {
                            if (record.receivedPackets == -1L && record.receivedBytes == -1L) {
                                record.receivedPackets = columns[0].toLong()
                                record.receivedBytes = columns[1].toLong()
                            }
                        } else {
                            if (record.sentPackets == -1L && record.sentBytes == -1L) {
                                record.sentPackets = columns[0].toLong()
                                record.sentBytes = columns[1].toLong()
                            }
                        }
                        if (oldRecord.id != null) {
                            check(records.put(key, record) == oldRecord)
                            oldRecords.put(oldRecord.id!!, oldRecord)
                        }
                    }
                    else -> check(false)
                }
            } catch (e: RuntimeException) {
                Timber.w(line)
                Timber.w(e)
            }
        }
        for ((_, record) in records) if (record.id == null) {
            check(record.sentPackets >= 0)
            check(record.sentBytes >= 0)
            check(record.receivedPackets >= 0)
            check(record.receivedBytes >= 0)
            AppDatabase.instance.trafficRecordDao.insert(record)
        }
        foregroundListeners(records.values, oldRecords)
    }
    fun update(timeout: Boolean = false) {
        synchronized(this) {
            if (timeout) scheduled = false
            if (records.isEmpty()) return
            val timestamp = System.currentTimeMillis()
            if (!timeout && timestamp - lastUpdate <= 100) return
            try {
                doUpdate(timestamp)
            } catch (e: RuntimeException) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }
            lastUpdate = timestamp
            scheduleUpdateLocked()
        }
    }

    fun clean() = synchronized(this) {
        update()
        unscheduleUpdateLocked()
        DebugHelper.log(TAG, "Cleaning records")
        records.clear()
    }

    /**
     * Possibly inefficient. Don't call this too often.
     */
    fun isWorking(mac: Long) = records.values.any { it.mac == mac }
}
