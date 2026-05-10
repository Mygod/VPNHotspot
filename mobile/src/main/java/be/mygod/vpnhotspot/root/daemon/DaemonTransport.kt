package be.mygod.vpnhotspot.root.daemon

import android.os.RemoteException
import be.mygod.vpnhotspot.root.daemon.proto.DaemonProto
import be.mygod.vpnhotspot.util.CrashlyticsKeyProvider
import com.google.firebase.crashlytics.CustomKeysAndValues
import java.io.IOException

object DaemonTransport {
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
        cause: Throwable = DaemonReportException(report),
    ) : RemoteException(report.toExceptionMessage()), CrashlyticsKeyProvider {
        init {
            initCause(cause)
        }

        fun withCurrentTrace() = DaemonException(report, callId, this)

        override val crashlyticsKeys get() = CustomKeysAndValues.Builder().apply {
            for ((key, value) in report.crashlyticsKeyValues) putString(key, value)
            if (callId != null) putString("daemon.callId", callId.toString())
        }.build()
    }

    private class DaemonReportException(report: DaemonErrorReport) : IOException(report.toExceptionMessage()) {
        init {
            stackTrace = arrayOf(StackTraceElement("vpnhotspotd", report.context, report.file, report.line))
        }
    }

    sealed class ReplyPayload {
        data object Ack : ReplyPayload()
        data class TrafficCounterLines(val lines: List<String>) : ReplyPayload()
    }

    sealed class EventPayload {
        data object Ack : EventPayload()
        data class NeighbourDeltas(val deltas: DaemonProto.NeighbourDeltas) : EventPayload()
    }

    sealed class Frame {
        data class Reply(val id: Long, val payload: ReplyPayload) : Frame()
        data class Event(val id: Long, val payload: EventPayload) : Frame()
        data class Error(val id: Long, val exception: DaemonException) : Frame()
        data class NonFatal(val id: Long?, val exception: DaemonException) : Frame()
        data class Complete(val id: Long) : Frame()
    }

    fun call(id: Long, command: DaemonProtocol.Command) = command.packet(id)

    fun readFrame(packet: ByteArray): Frame {
        val envelope = DaemonProto.DaemonEnvelope.parseFrom(packet)
        return when (envelope.frameCase) {
            DaemonProto.DaemonEnvelope.FrameCase.REPLY -> envelope.reply.let {
                Frame.Reply(it.callId.readCallId(), it.readPayload())
            }
            DaemonProto.DaemonEnvelope.FrameCase.EVENT -> envelope.event.let {
                Frame.Event(it.callId.readCallId(), it.readPayload())
            }
            DaemonProto.DaemonEnvelope.FrameCase.ERROR -> envelope.error.let {
                val id = it.callId.readCallId()
                if (!it.hasReport()) throw IOException("Missing daemon error report")
                Frame.Error(id, DaemonException(it.report.toDaemonErrorReport(), id))
            }
            DaemonProto.DaemonEnvelope.FrameCase.NON_FATAL -> envelope.nonFatal.let {
                val id = if (it.hasCallId()) it.callId.readCallId() else null
                if (!it.hasReport()) throw IOException("Missing daemon nonfatal report")
                Frame.NonFatal(id, DaemonException(it.report.toDaemonErrorReport(), id))
            }
            DaemonProto.DaemonEnvelope.FrameCase.COMPLETE -> envelope.complete.let {
                Frame.Complete(it.callId.readCallId())
            }
            DaemonProto.DaemonEnvelope.FrameCase.FRAME_NOT_SET -> throw IOException("Missing daemon frame")
        }
    }

    private fun DaemonProto.ReplyFrame.readPayload() = when (payloadCase) {
        DaemonProto.ReplyFrame.PayloadCase.ACK -> ReplyPayload.Ack
        DaemonProto.ReplyFrame.PayloadCase.TRAFFIC_COUNTER_LINES ->
            ReplyPayload.TrafficCounterLines(trafficCounterLines.linesList)
        DaemonProto.ReplyFrame.PayloadCase.PAYLOAD_NOT_SET -> throw IOException("Missing daemon reply payload")
    }

    private fun DaemonProto.EventFrame.readPayload() = when (payloadCase) {
        DaemonProto.EventFrame.PayloadCase.ACK -> EventPayload.Ack
        DaemonProto.EventFrame.PayloadCase.NEIGHBOUR_DELTAS -> EventPayload.NeighbourDeltas(neighbourDeltas)
        DaemonProto.EventFrame.PayloadCase.PAYLOAD_NOT_SET -> throw IOException("Missing daemon event payload")
    }

    private fun Long.readCallId(): Long {
        if (this <= 0) throw IOException("Invalid daemon call id $this")
        return this
    }

    private fun DaemonProto.DaemonErrorReport.toDaemonErrorReport() = DaemonErrorReport(
        context = context,
        message = message,
        errno = if (hasErrno()) errno else null,
        kind = kind,
        file = file,
        line = line,
        column = column,
        pid = pid,
        details = detailsList.associate { it.key to it.value },
    )

    private fun DaemonErrorReport.toExceptionMessage() = buildString {
        append(context).append(": ").append(message)
        if (errno != null) append(" (errno=").append(errno).append(')')
        append(" [")
        if (errno == null || kind != "Uncategorized") append(kind).append(" at ")
        append(file).append(':').append(line).append(':')
            .append(column).append(", pid=").append(pid).append(']')
    }
}
