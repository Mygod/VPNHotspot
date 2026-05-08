package be.mygod.vpnhotspot.root.daemon

import be.mygod.vpnhotspot.util.CrashlyticsKeyProvider
import com.google.firebase.crashlytics.CustomKeysAndValues
import kotlinx.io.Buffer
import kotlinx.io.Sink
import kotlinx.io.Source
import kotlinx.io.readByteArray
import kotlinx.io.readString
import java.io.IOException

object DaemonTransport {
    private const val FRAME_CALL = 0
    private const val FRAME_CANCEL = 1

    private const val FRAME_REPLY = 0
    private const val FRAME_EVENT = 1
    private const val FRAME_ERROR = 2
    private const val FRAME_NON_FATAL = 3

    private const val NO_CALL_ID = 0L
    private val INVALID_CRASHLYTICS_KEY_CHAR = Regex("[^A-Za-z0-9_.-]")

    private fun sanitizeCrashlyticsKey(key: String) =
        INVALID_CRASHLYTICS_KEY_CHAR.replace(key, "_").ifEmpty { "detail" }.take(64)

    data class DaemonErrorReport(
        val context: String,
        val message: String,
        val errno: Int?,
        val kind: String,
        val file: String,
        val line: Int,
        val column: Int,
        val pid: Int,
        val details: Map<String, String>,
    ) {
        val crashlyticsKeyValues get() = buildMap {
            for ((key, value) in details) put("daemon.${sanitizeCrashlyticsKey(key)}", value)
        }
    }

    class DaemonException(
        val report: DaemonErrorReport,
        private val callId: Long? = null,
    ) : IOException(report.toExceptionMessage()), CrashlyticsKeyProvider {
        init {
            stackTrace = arrayOf(StackTraceElement("vpnhotspotd", report.context, report.file, report.line))
        }

        override val crashlyticsKeys get() = CustomKeysAndValues.Builder().apply {
            for ((key, value) in report.crashlyticsKeyValues) putString(key, value)
            if (callId != null) putString("daemon.callId", callId.toString())
        }.build()
    }

    sealed class Frame {
        data class Reply(val id: Long, val packet: ByteArray) : Frame()
        data class Event(val id: Long, val packet: ByteArray) : Frame()
        data class Error(val id: Long, val exception: DaemonException) : Frame()
        data class NonFatal(val id: Long?, val exception: DaemonException) : Frame()
    }

    fun call(id: Long, packet: ByteArray) = writeFrame(FRAME_CALL, id) { write(packet) }

    fun cancel(id: Long) = writeFrame(FRAME_CANCEL, id) { }

    fun readFrame(packet: ByteArray): Frame {
        val input = Buffer().apply { write(packet) }
        return when (val type = input.readByte().toInt() and 0xFF) {
            FRAME_REPLY -> Frame.Reply(input.readCallId(), input.readByteArray())
            FRAME_EVENT -> Frame.Event(input.readCallId(), input.readByteArray())
            FRAME_ERROR -> {
                val id = input.readCallId()
                Frame.Error(id, DaemonException(input.readErrorReport(), id))
            }
            FRAME_NON_FATAL -> {
                val id = input.readLong().let { if (it == NO_CALL_ID) null else it }
                if (id != null && id < 0) throw IOException("Invalid daemon nonfatal call id $id")
                Frame.NonFatal(id, DaemonException(input.readErrorReport(), id))
            }
            else -> throw IOException("Unknown daemon frame type $type")
        }
    }

    private fun writeFrame(type: Int, id: Long, block: Sink.() -> Unit): ByteArray {
        require(id > 0) { "Invalid daemon call id $id" }
        val output = Buffer()
        output.writeByte(type.toByte())
        output.writeLong(id)
        output.block()
        return output.readByteArray()
    }

    private fun Source.readCallId(): Long {
        val id = readLong()
        if (id <= 0) throw IOException("Invalid daemon call id $id")
        return id
    }

    private fun Source.readErrorReport(): DaemonErrorReport {
        val context = readUtf()
        val message = readUtf()
        val errno = readInt().let { if (it < 0) null else it }
        val kind = readUtf()
        val file = readUtf()
        val line = readInt()
        val column = readInt()
        val pid = readInt()
        val detailCount = readCount("error detail")
        return DaemonErrorReport(context, message, errno, kind, file, line, column, pid, buildMap {
            repeat(detailCount) { put(readUtf(), readUtf()) }
        })
    }

    private fun DaemonErrorReport.toExceptionMessage() = buildString {
        append(context).append(": ").append(message)
        if (errno != null) append(" (errno=").append(errno).append(')')
        append(" [").append(kind).append(" at ").append(file).append(':').append(line).append(':')
            .append(column).append(", pid=").append(pid).append(']')
    }

    private fun Source.readUtf(): String {
        val length = readInt()
        if (length < 0) throw IOException("Invalid string length $length")
        return readString(length.toLong())
    }

    private fun Source.readCount(name: String): Int {
        val count = readInt()
        if (count < 0) throw IOException("Invalid $name count $count")
        return count
    }
}
