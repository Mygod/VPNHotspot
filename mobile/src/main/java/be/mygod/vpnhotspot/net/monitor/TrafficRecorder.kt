package be.mygod.vpnhotspot.net.monitor

import android.net.MacAddress
import androidx.collection.LongObjectMap
import androidx.collection.MutableObjectList
import androidx.collection.MutableLongObjectMap
import androidx.collection.MutableScatterMap
import androidx.collection.MutableScatterSet
import androidx.collection.ObjectList
import be.mygod.vpnhotspot.root.daemon.DaemonTrafficSource
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.TrafficCounter
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
import kotlinx.coroutines.flow.onCompletion
import kotlinx.coroutines.flow.onSubscription
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import timber.log.Timber
import java.net.InetAddress
import java.util.concurrent.TimeUnit

object TrafficRecorder {
    private const val FOREGROUND_POLL_MS = 1015L

    data class ForegroundUpdate(
        val newRecords: ObjectList<TrafficRecord>,
        val oldRecords: LongObjectMap<TrafficRecord>,
    )

    private data class ClientKey(val mac: MacAddress, val downstream: String)
    data class CounterSource(val ip: InetAddress, val upstream: String?)
    data class CounterKey(
        val mac: MacAddress,
        val downstream: String,
        val source: CounterSource,
    )

    private var lastUpdate = 0L
    private val activeClients = MutableScatterSet<ClientKey>()
    private val records = MutableScatterMap<CounterKey, TrafficRecord>()
    private val updateMutex = Mutex()   // owns recorder state below and updateJob
    private val foregroundUpdatesState = MutableSharedFlow<ForegroundUpdate>(
        extraBufferCapacity = 1,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    val foregroundUpdates = foregroundUpdatesState
        .onSubscription { rescheduleUpdate() }
        .onCompletion { rescheduleUpdate() }

    suspend fun register(ip: InetAddress, downstream: String, mac: MacAddress) = updateMutex.withLock {
        val key = CounterKey(mac, downstream, CounterSource(ip, null))
        check(records[key] == null)
        val record = TrafficRecord(mac = mac, ip = ip, downstream = downstream)
        AppDatabase.instance.trafficRecordDao.insert(record)
        Timber.d("Registering $key")
        check(records.put(key, record) == null)
        scheduleUpdateLocked()
    }
    suspend fun register(mac: MacAddress, downstream: String) = updateMutex.withLock {
        Timber.d("Registering ${ClientKey(mac, downstream)}")
        activeClients.add(ClientKey(mac, downstream))
        scheduleUpdateLocked()
    }
    suspend fun unregister(ip: InetAddress, downstream: String) = updateMutex.withLock {
        var removed = false
        records.removeIf { key, _ ->
            (key.downstream == downstream && key.source.ip == ip && key.source.upstream == null).also {
                if (it) {
                    Timber.d("Unregistering $key")
                    removed = true
                }
            }
        }
        if (!removed) Timber.w("Failed to find traffic record for ${ip to downstream}.")
    }
    suspend fun unregister(mac: MacAddress, downstream: String) = updateMutex.withLock {
        val clientKey = ClientKey(mac, downstream)
        Timber.d("Unregistering $clientKey")
        if (!activeClients.remove(clientKey)) Timber.w("Failed to find traffic client for $clientKey.")
        records.removeIf { key, _ -> key.mac == mac && key.downstream == downstream && key.source.upstream != null }
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
            update()
        }
    }

    private suspend fun rescheduleUpdate() = updateMutex.withLock {
        unscheduleUpdateLocked()
        scheduleUpdateLocked()
    }

    suspend fun update() = updateMutex.withLock { updateLocked() }
    private suspend fun updateLocked() {
        if (updateJob === currentCoroutineContext()[Job]) updateJob = null else unscheduleUpdateLocked()
        if (records.isEmpty() && activeClients.isEmpty()) return
        try {
            val counters = DaemonController.readTrafficCounters()
            val timestamp = System.currentTimeMillis()
            lastUpdate = timestamp
            val oldRecords = MutableLongObjectMap<TrafficRecord>()
            val seenKeys = MutableScatterSet<CounterKey>()
            loop@ for (counter in counters) {
                try {
                    val key = counterKey(counter) ?: run {
                        Timber.w("Ignoring unsupported traffic counter source: $counter")
                        continue@loop
                    }
                    seenKeys.add(key)
                    val oldRecord = records[key]
                    val record = if (oldRecord == null) {
                        if (key.source.upstream == null) continue@loop
                        if (counter.sent_packets == 0L && counter.sent_bytes == 0L &&
                            counter.received_packets == 0L && counter.received_bytes == 0L) continue@loop
                        TrafficRecord(
                            timestamp = timestamp,
                            mac = key.mac,
                            ip = key.source.ip,
                            upstream = key.source.upstream,
                            downstream = key.downstream,
                            sentPackets = counter.sent_packets,
                            sentBytes = counter.sent_bytes,
                            receivedPackets = counter.received_packets,
                            receivedBytes = counter.received_bytes,
                        )
                    } else if (oldRecord.id == null) oldRecord.apply {
                        sentPackets = counter.sent_packets
                        sentBytes = counter.sent_bytes
                        receivedPackets = counter.received_packets
                        receivedBytes = counter.received_bytes
                    } else TrafficRecord(
                        timestamp = timestamp,
                        mac = oldRecord.mac,
                        ip = oldRecord.ip,
                        upstream = oldRecord.upstream,
                        downstream = oldRecord.downstream,
                        sentPackets = counter.sent_packets,
                        sentBytes = counter.sent_bytes,
                        receivedPackets = counter.received_packets,
                        receivedBytes = counter.received_bytes,
                        previousId = oldRecord.id,
                    )
                    if (oldRecord == null) {
                        records[key] = record
                    } else oldRecord.id?.let { oldId ->
                        check(records.put(key, record) == oldRecord)
                        oldRecords[oldId] = oldRecord
                    }
                } catch (e: Exception) {
                    Timber.w(counter.toString())
                    Timber.w(e)
                }
            }
            records.removeIf { key, _ -> key.source.upstream != null && key !in seenKeys }
            records.forEachValue { record ->
                if (record.id != null) return@forEachValue
                check(record.sentPackets >= 0)
                check(record.sentBytes >= 0)
                check(record.receivedPackets >= 0)
                check(record.receivedBytes >= 0)
                AppDatabase.instance.trafficRecordDao.insert(record)
            }
            val newRecords = MutableObjectList<TrafficRecord>(records.size)
            records.forEachValue { newRecords.add(it) }
            foregroundUpdatesState.tryEmit(ForegroundUpdate(newRecords, oldRecords))
        } catch (_: CancellationException) {
        } catch (e: Exception) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
        }
        if (records.isNotEmpty() || activeClients.isNotEmpty()) scheduleUpdateLocked()
    }

    suspend fun clean() = updateMutex.withLock {
        updateLocked()
        unscheduleUpdateLocked()
        Timber.d("Cleaning records")
        activeClients.clear()
        records.clear()
    }

    /**
     * Possibly inefficient. Don't call this too often.
     */
    suspend fun isWorking(mac: MacAddress) = updateMutex.withLock {
        activeClients.forEach { if (it.mac == mac) return@withLock true }
        records.forEach { key, _ -> if (key.mac == mac) return@withLock true }
        false
    }

    fun counterKey(counter: TrafficCounter): CounterKey? {
        val recordSource = counterSource(counter) ?: return null
        val mac = counter.mac.takeIf { it.size == 6 }?.toByteArray() ?: return null
        return CounterKey(MacAddress.fromBytes(mac), counter.downstream, recordSource)
    }

    fun counterSource(counter: TrafficCounter): CounterSource? {
        val counterSource = counter.source
        return counterSource?.ipv4_forward_address?.let {
            if (it.size != 4) return@let null
            val ip = InetAddress.getByAddress(it.toByteArray())
            CounterSource(ip, null)
        } ?: counterSource?.daemon_source?.marker?.let { CounterSource(TrafficRecord.DAEMON_SOURCE_ADDRESS, it) }
    }

    private val DaemonTrafficSource.marker get() = when (this) {
        DaemonTrafficSource.DAEMON_TRAFFIC_SOURCE_DNS -> TrafficRecord.DAEMON_SOURCE_DNS
        DaemonTrafficSource.DAEMON_TRAFFIC_SOURCE_NAT66_TCP -> TrafficRecord.DAEMON_SOURCE_NAT66_TCP
        DaemonTrafficSource.DAEMON_TRAFFIC_SOURCE_NAT66_UDP -> TrafficRecord.DAEMON_SOURCE_NAT66_UDP
        DaemonTrafficSource.DAEMON_TRAFFIC_SOURCE_NAT66_ICMPV6 -> TrafficRecord.DAEMON_SOURCE_NAT66_ICMPV6
        else -> null
    }
}
