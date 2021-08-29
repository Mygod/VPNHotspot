package be.mygod.vpnhotspot.net.monitor

import androidx.collection.LongSparseArray
import androidx.collection.set
import be.mygod.vpnhotspot.net.IpDev
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.net.Routing.Companion.IPTABLES
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.TrafficRecord
import be.mygod.vpnhotspot.util.Event2
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.util.parseNumericAddress
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.*
import timber.log.Timber
import java.net.InetAddress
import java.util.concurrent.TimeUnit

object TrafficRecorder {
    private const val ANYWHERE = "0.0.0.0/0"

    private var lastUpdate = 0L
    private val records = mutableMapOf<IpDev, TrafficRecord>()
    val foregroundListeners = Event2<Collection<TrafficRecord>, LongSparseArray<TrafficRecord>>()

    fun register(ip: InetAddress, downstream: String, mac: MacAddressCompat) {
        val record = TrafficRecord(mac = mac.addr, ip = ip, downstream = downstream)
        AppDatabase.instance.trafficRecordDao.insert(record)
        synchronized(this) {
            val key = IpDev(ip, downstream)
            Timber.d("Registering $key")
            check(records.putIfAbsent(key, record) == null)
            scheduleUpdateLocked()
        }
    }
    fun unregister(ip: InetAddress, downstream: String) = synchronized(this) {
        update()    // flush stats before removing
        val key = IpDev(ip, downstream)
        Timber.d("Unregistering $key")
        if (records.remove(key) == null) Timber.w("Failed to find traffic record for $key.")
    }

    private var updateJob: Job? = null
    private fun unscheduleUpdateLocked() {
        updateJob?.cancel()
        updateJob = null
    }
    private fun scheduleUpdateLocked() {
        if (updateJob != null) return
        val now = System.currentTimeMillis()
        val minute = TimeUnit.MINUTES.toMillis(1)
        var timeout = minute - now % minute
        if (foregroundListeners.isNotEmpty() && timeout > 1000) timeout = 1000
        updateJob = GlobalScope.launch(start = CoroutineStart.UNDISPATCHED) {
            delay(timeout)
            update(true)
        }
    }

    fun rescheduleUpdate() = synchronized(this) {
        unscheduleUpdateLocked()
        scheduleUpdateLocked()
    }

    private fun doUpdate(timestamp: Long) {
        val oldRecords = LongSparseArray<TrafficRecord>()
        loop@ for (line in RootSession.use {
            val command = "$IPTABLES -nvx -L vpnhotspot_acl"
            val result = it.execQuiet(command)
            val message = result.message(listOf(command))
            if (result.err.isNotEmpty()) Timber.i(message)
            result.out.lineSequence().drop(2)
        }) {
            if (line.isBlank()) continue
            val columns = line.split("\\s+".toRegex()).filter { it.isNotEmpty() }
            try {
                check(columns.size >= 9)
                when (columns[2]) {
                    "DROP" -> { }
                    "ACCEPT" -> {
                        val isReceive = columns[7] == ANYWHERE
                        val isSend = columns[8] == ANYWHERE
                        // this check might fail when the user performed an upgrade from 1.x
                        check(isReceive != isSend) { "Failed to set up blocking rules, please clean routing rules" }
                        val ip = parseNumericAddress(columns[if (isReceive) 8 else 7])
                        val downstream = columns[if (isReceive) 6 else 5]
                        val key = IpDev(ip, downstream)
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
                        oldRecord.id?.let { oldId ->
                            check(records.put(key, record) == oldRecord)
                            oldRecords[oldId] = oldRecord
                        }
                    }
                    else -> check(false)
                }
            } catch (e: Exception) {
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
        foregroundListeners(records.values.toList(), oldRecords)
    }
    fun update(timeout: Boolean = false) {
        synchronized(this) {
            unscheduleUpdateLocked()
            if (records.isEmpty()) return
            val timestamp = System.currentTimeMillis()
            if (!timeout && timestamp - lastUpdate <= 100) return
            try {
                doUpdate(timestamp)
            } catch (_: CancellationException) {
            } catch (e: Exception) {
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
        Timber.d("Cleaning records")
        records.clear()
    }

    /**
     * Possibly inefficient. Don't call this too often.
     */
    fun isWorking(mac: MacAddressCompat) = records.values.any { it.mac == mac.addr }
}
