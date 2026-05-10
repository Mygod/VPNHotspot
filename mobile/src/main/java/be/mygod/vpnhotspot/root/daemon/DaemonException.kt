package be.mygod.vpnhotspot.root.daemon

import android.os.RemoteException
import be.mygod.vpnhotspot.util.CrashlyticsKeyProvider
import com.google.firebase.crashlytics.CustomKeysAndValues
import java.io.IOException

private val INVALID_CRASHLYTICS_KEY_CHAR = Regex("[^A-Za-z0-9_.-]")

private fun sanitizeCrashlyticsKey(key: String) =
    INVALID_CRASHLYTICS_KEY_CHAR.replace(key, "_").ifEmpty { "detail" }.take(64)

class DaemonException(
    val report: DaemonProto.DaemonErrorReport,
    private val callId: Long? = null,
    cause: Throwable = DaemonReportException(report),
) : RemoteException(report.toExceptionMessage()), CrashlyticsKeyProvider {
    init {
        initCause(cause)
    }

    fun withCurrentTrace() = DaemonException(report, callId, this)

    override val crashlyticsKeys get() = CustomKeysAndValues.Builder().apply {
        for (detail in report.detailsList) putString("daemon.${sanitizeCrashlyticsKey(detail.key)}", detail.value)
        if (callId != null) putString("daemon.callId", callId.toString())
    }.build()
}

private class DaemonReportException(report: DaemonProto.DaemonErrorReport) : IOException(report.toExceptionMessage()) {
    init {
        stackTrace = arrayOf(StackTraceElement("vpnhotspotd", report.context, report.file, report.line))
    }
}

private fun DaemonProto.DaemonErrorReport.toExceptionMessage() = buildString {
    append(context).append(": ").append(message)
    if (hasErrno()) append(" (errno=").append(errno).append(')')
    append(" [")
    if (!hasErrno() || kind != "Uncategorized") append(kind).append(" at ")
    append(file).append(':').append(line).append(':')
        .append(column).append(", pid=").append(pid).append(']')
}
