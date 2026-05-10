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

    @Test
    fun daemonExceptionMessageDoesNotRepeatOsErrorCode() {
        val e = DaemonException(daemonErrorReport(
            message = "Connection timed out (os error 110)",
            errno = 110,
            kind = "TimedOut",
        ))

        assertDaemonReportCause(e, "routing.command: Connection timed out (os error 110) " +
                "[TimedOut at routing.rs:123:45, pid=2345]")
    }

    private fun daemonErrorReport(
        message: String = "Device or resource busy",
        errno: Int? = 16,
        kind: String = "ResourceBusy",
    ) = DaemonErrorReport(
        context = "routing.command",
        message = message,
        errno = errno,
        kind = kind,
        file_ = "routing.rs",
        line = 123,
        column = 45,
        pid = 2345,
        details = listOf(ErrorDetail("command", "iptables-restore")),
    )

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
