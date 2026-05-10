package be.mygod.vpnhotspot.root.daemon

import be.mygod.vpnhotspot.root.daemon.proto.DaemonProto
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

class DaemonProtocolTest {
    @Test
    fun readFrameDecodesStructuredError() {
        val report = daemonErrorReport()
        val frame = DaemonTransport.readFrame(DaemonProto.DaemonEnvelope.newBuilder()
            .setError(DaemonProto.ErrorFrame.newBuilder().setCallId(123).setReport(report.toProto()))
            .build()
            .toByteArray())

        assertTrue(frame is DaemonTransport.Frame.Error)
        val e = (frame as DaemonTransport.Frame.Error).exception
        assertEquals(16, e.report.errno)
        assertEquals(report, e.report)
        assertDaemonReportCause(e, "routing.command: Device or resource busy (errno=16) " +
                "[ResourceBusy at routing.rs:123:45, pid=2345]")
        assertEquals(setOf("daemon.command"), e.report.crashlyticsKeyValues.keys)
        assertEquals("iptables-restore", e.report.crashlyticsKeyValues["daemon.command"])
    }

    @Test
    fun daemonExceptionMessageHidesUncategorizedWhenErrnoIsPresent() {
        val e = DaemonTransport.DaemonException(daemonErrorReport(
            message = "No such device",
            errno = 19,
            kind = "Uncategorized",
        ))

        assertEquals("routing.command: No such device (errno=19) [routing.rs:123:45, pid=2345]",
            e.cause!!.message)
        assertEquals("Uncategorized", e.report.kind)
    }

    @Test
    fun readFrameDecodesNonFatalErrorWithoutCallId() {
        val report = daemonErrorReport()
        val frame = DaemonTransport.readFrame(DaemonProto.DaemonEnvelope.newBuilder()
            .setNonFatal(DaemonProto.NonFatalFrame.newBuilder().setReport(report.toProto()))
            .build()
            .toByteArray())

        assertTrue(frame is DaemonTransport.Frame.NonFatal)
        val e = (frame as DaemonTransport.Frame.NonFatal).exception
        assertEquals(null, frame.id)
        assertEquals(report, e.report)
        assertDaemonReportCause(e)
    }

    private fun daemonErrorReport(
        message: String = "Device or resource busy",
        errno: Int? = 16,
        kind: String = "ResourceBusy",
    ) = DaemonTransport.DaemonErrorReport(
        context = "routing.command",
        message = message,
        errno = errno,
        kind = kind,
        file = "routing.rs",
        line = 123,
        column = 45,
        pid = 2345,
        details = mapOf("command" to "iptables-restore"),
    )

    private fun DaemonTransport.DaemonErrorReport.toProto() = DaemonProto.DaemonErrorReport.newBuilder().also {
        it.context = context
        it.message = message
        errno?.let(it::setErrno)
        it.kind = kind
        it.file = file
        it.line = line
        it.column = column
        it.pid = pid
        it.addAllDetails(details.map { (key, value) ->
            DaemonProto.ErrorDetail.newBuilder().setKey(key).setValue(value).build()
        })
    }.build()

    private fun assertDaemonReportCause(e: DaemonTransport.DaemonException, message: String? = null) {
        assertNotNull(e.cause)
        if (message != null) assertEquals(message, e.cause!!.message)
        val frame = e.cause!!.stackTrace.single()
        assertEquals("vpnhotspotd", frame.className)
        assertEquals("routing.command", frame.methodName)
        assertEquals("routing.rs", frame.fileName)
        assertEquals(123, frame.lineNumber)
    }
}
