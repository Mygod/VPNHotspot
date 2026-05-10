package be.mygod.vpnhotspot.root.daemon

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Test

class DaemonExceptionTest {
    @Test
    fun daemonExceptionProjectsGeneratedReport() {
        val report = daemonErrorReport()
        val e = DaemonException(report, 123)

        assertEquals(report, e.report)
        assertDaemonReportCause(e, "routing.command: Device or resource busy (errno=16) " +
                "[ResourceBusy at routing.rs:123:45, pid=2345]")
    }

    @Test
    fun daemonExceptionMessageHidesUncategorizedWhenErrnoIsPresent() {
        val e = DaemonException(daemonErrorReport(
            message = "No such device",
            errno = 19,
            kind = "Uncategorized",
        ))

        assertEquals("routing.command: No such device (errno=19) [routing.rs:123:45, pid=2345]",
            e.cause!!.message)
        assertEquals("Uncategorized", e.report.kind)
    }

    private fun daemonErrorReport(
        message: String = "Device or resource busy",
        errno: Int? = 16,
        kind: String = "ResourceBusy",
    ) = DaemonProto.DaemonErrorReport.newBuilder().also {
        it.context = "routing.command"
        it.message = message
        errno?.let(it::setErrno)
        it.kind = kind
        it.file = "routing.rs"
        it.line = 123
        it.column = 45
        it.pid = 2345
        it.addDetails(DaemonProto.ErrorDetail.newBuilder().setKey("command").setValue("iptables-restore"))
    }.build()

    private fun assertDaemonReportCause(e: DaemonException, message: String) {
        assertNotNull(e.cause)
        assertEquals(message, e.cause!!.message)
        val frame = e.cause!!.stackTrace.single()
        assertEquals("vpnhotspotd", frame.className)
        assertEquals("routing.command", frame.methodName)
        assertEquals("routing.rs", frame.fileName)
        assertEquals(123, frame.lineNumber)
    }
}
