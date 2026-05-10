package be.mygod.vpnhotspot.root.daemon

import android.os.RemoteException
import be.mygod.vpnhotspot.util.CrashlyticsKeyProvider
import com.google.firebase.crashlytics.CustomKeysAndValues
import java.io.IOException

class DaemonException(
    val report: DaemonErrorReport,
    private val callId: Long? = null,
    cause: Throwable = ReportException(report),
) : RemoteException(report.toExceptionMessage()), CrashlyticsKeyProvider {
    companion object {
        private val INVALID_CRASHLYTICS_KEY_CHAR = Regex("[^A-Za-z0-9_.-]")

        private fun sanitizeCrashlyticsKey(key: String) =
            INVALID_CRASHLYTICS_KEY_CHAR.replace(key, "_").ifEmpty { "detail" }.take(64)

        private fun DaemonErrorReport.toExceptionMessage() = buildString {
            append(context).append(": ").append(message)
            if (errno != null) append(" (errno=").append(errno).append(')')
            append(" [")
            if (errno == null || kind != "Uncategorized") append(kind).append(" at ")
            append(file_).append(':').append(line).append(':')
                .append(column).append(", pid=").append(pid).append(']')
        }
    }

    private class ReportException(report: DaemonErrorReport) : IOException(report.toExceptionMessage()) {
        init {
            stackTrace = arrayOf(StackTraceElement("vpnhotspotd", report.context, report.file_, report.line))
        }
    }

    init {
        initCause(cause)
    }

    fun withCurrentTrace() = DaemonException(report, callId, this)

    override val crashlyticsKeys get() = CustomKeysAndValues.Builder().apply {
        for (detail in report.details) putString("daemon.${sanitizeCrashlyticsKey(detail.key)}", detail.value_)
        if (callId != null) putString("daemon.callId", callId.toString())
    }.build()
}
