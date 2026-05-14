package be.mygod.vpnhotspot.net.monitor

import android.net.MacAddress
import android.net.InetAddresses
import androidx.collection.LongObjectMap
import androidx.collection.MutableObjectList
import androidx.collection.MutableLongObjectMap
import androidx.collection.MutableScatterMap
import androidx.collection.ObjectList
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.TrafficRecord
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import timber.log.Timber
import java.net.InetAddress
import java.util.concurrent.TimeUnit

object TrafficRecorder {
    private const val ANYWHERE = "0.0.0.0/0"
    private const val FOREGROUND_POLL_MS = 1015L

    data class ForegroundUpdate(
        val newRecords: ObjectList<TrafficRecord>,
        val oldRecords: LongObjectMap<TrafficRecord>,
    )

    private var lastUpdate = 0L
    private val records = MutableScatterMap<Pair<InetAddress, String>, TrafficRecord>()
    private val updateMutex = Mutex()
    private val foregroundUpdatesState = MutableSharedFlow<ForegroundUpdate>(
        extraBufferCapacity = 1,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    val foregroundUpdates = foregroundUpdatesState.asSharedFlow()

    fun register(ip: InetAddress, downstream: String, mac: MacAddress) {
        val record = TrafficRecord(mac = mac, ip = ip, downstream = downstream)
        AppDatabase.instance.trafficRecordDao.insert(record)
        synchronized(this) {
            val key = ip to downstream
            Timber.d("Registering $key")
            records.compute(key) { _, old ->
                check(old == null)
                record
            }
            scheduleUpdateLocked()
        }
    }
    suspend fun unregister(ip: InetAddress, downstream: String) {
        update()    // flush stats before removing
        synchronized(this) {
            val key = ip to downstream
            Timber.d("Unregistering $key")
            if (records.remove(key) == null) Timber.w("Failed to find traffic record for $key.")
        }
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
        if (foregroundUpdatesState.subscriptionCount.value > 0 && timeout > FOREGROUND_POLL_MS) {
            timeout = FOREGROUND_POLL_MS
        }
        updateJob = GlobalScope.launch(start = CoroutineStart.UNDISPATCHED) {
            delay(timeout)
            update(true)
        }
    }

    fun rescheduleUpdate() = synchronized(this) {
        unscheduleUpdateLocked()
        scheduleUpdateLocked()
    }

    private suspend fun doUpdate(timestamp: Long) {
        val lines = DaemonController.readTrafficCounterLines()
        synchronized(this) {
            val oldRecords = MutableLongObjectMap<TrafficRecord>()
            loop@ for (line in lines) {
                if (line.isBlank()) continue
                val columns = line.split("\\s+".toRegex()).filter { it.isNotEmpty() }
                try {
                    check(columns.size >= 9)
                    when (columns[2]) {
                        "DROP" -> { }
                        "RETURN" -> {
                            val isReceive = columns[7] == ANYWHERE
                            val isSend = columns[8] == ANYWHERE
                            // this check might fail when the user performed an upgrade from 1.x
                            check(isReceive != isSend) {
                                "Failed to set up traffic accounting rules, please clean routing rules"
                            }
                            val ip = InetAddresses.parseNumericAddress(columns[if (isReceive) 8 else 7])
                            val downstream = columns[if (isReceive) 6 else 5]
                            val key = ip to downstream
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
            records.forEachValue { record ->
                if (record.id == null) {
                    check(record.sentPackets >= 0)
                    check(record.sentBytes >= 0)
                    check(record.receivedPackets >= 0)
                    check(record.receivedBytes >= 0)
                    AppDatabase.instance.trafficRecordDao.insert(record)
                }
            }
            val newRecords = MutableObjectList<TrafficRecord>(records.size)
            records.forEachValue { newRecords.add(it) }
            foregroundUpdatesState.tryEmit(ForegroundUpdate(newRecords, oldRecords))
        }
    }
    suspend fun update(timeout: Boolean = false) {
        updateMutex.withLock {
            val currentJob = currentCoroutineContext()[Job]
            val timestamp = synchronized(this) {
                if (updateJob === currentJob) updateJob = null else unscheduleUpdateLocked()
                if (records.isEmpty()) return@withLock
                val timestamp = System.currentTimeMillis()
                if (!timeout && timestamp - lastUpdate <= 100) return@withLock
                timestamp
            }
            try {
                doUpdate(timestamp)
            } catch (_: CancellationException) {
            } catch (e: Exception) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }
            synchronized(this) {
                lastUpdate = timestamp
                if (records.isNotEmpty()) scheduleUpdateLocked()
            }
        }
    }

    suspend fun clean() {
        update()
        synchronized(this) {
            unscheduleUpdateLocked()
            Timber.d("Cleaning records")
            records.clear()
        }
    }

    /**
     * Possibly inefficient. Don't call this too often.
     */
    fun isWorking(mac: MacAddress) = synchronized(this) {
        records.forEachValue { if (it.mac == mac) return@synchronized true }
        false
    }
}
