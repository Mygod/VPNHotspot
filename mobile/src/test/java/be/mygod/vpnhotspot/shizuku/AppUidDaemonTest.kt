package be.mygod.vpnhotspot.shizuku

import be.mygod.vpnhotspot.root.daemon.DaemonEnvelope
import be.mygod.vpnhotspot.root.daemon.DaemonErrorReport
import be.mygod.vpnhotspot.root.daemon.DaemonException
import be.mygod.vpnhotspot.root.daemon.DaemonIpc
import be.mygod.vpnhotspot.root.daemon.ErrorFrame
import io.ktor.utils.io.ByteChannel
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test
import java.io.IOException

class AppUidDaemonTest {
    @Test
    fun configWriteFailureAwaitsStructuredDaemonError() = runBlocking {
        val input = ByteChannel()
        val output = ByteChannel().apply { cancel(IOException("EPIPE")) }
        val daemon = AppUidDaemon.create(input, output, this)
        val failure = async(start = CoroutineStart.UNDISPATCHED) {
            try {
                daemon.apply(true)
                null
            } catch (e: Exception) {
                e
            }
        }
        assertFalse(failure.isCompleted)

        val report = DaemonErrorReport(
            context = "shizuku.control.config",
            message = "invalid config",
            kind = "InvalidInput",
            file_ = "app_config.rs",
            line = 123,
            column = 45,
            pid = 2345,
        )
        DaemonIpc.writeFrame(input, DaemonEnvelope.ADAPTER.encode(DaemonEnvelope(
            error = ErrorFrame(call_id = 2, report = report))))

        assertEquals(report, (failure.await() as DaemonException).report)
    }
}
