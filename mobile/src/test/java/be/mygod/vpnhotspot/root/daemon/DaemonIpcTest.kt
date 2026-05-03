package be.mygod.vpnhotspot.root.daemon

import io.ktor.utils.io.ByteChannel
import io.ktor.utils.io.writeByte
import io.ktor.utils.io.writeInt
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertThrows
import org.junit.Test
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import java.io.EOFException
import java.io.IOException

class DaemonIpcTest {
    @Test
    fun readFrameHandlesSplitHeaderAndBody() = runBlocking {
        val input = ByteChannel()
        val writer = launch {
            for (byte in byteArrayOf(0, 0, 0, 3, 1, 2, 3)) {
                input.writeByte(byte)
                input.flush()
            }
            input.flushAndClose()
        }
        assertArrayEquals(byteArrayOf(1, 2, 3), DaemonIpc.readFrame(input))
        writer.join()
    }

    @Test
    fun readFrameHandlesMultipleFrames() = runBlocking {
        val channel = ByteChannel()
        DaemonIpc.writeFrame(channel, byteArrayOf(1, 2))
        DaemonIpc.writeFrame(channel, byteArrayOf(3))
        channel.flushAndClose()
        assertArrayEquals(byteArrayOf(1, 2), DaemonIpc.readFrame(channel))
        assertArrayEquals(byteArrayOf(3), DaemonIpc.readFrame(channel))
    }

    @Test
    fun readFrameRejectsUnexpectedEof() {
        assertThrows(EOFException::class.java) {
            runBlocking {
                val input = ByteChannel()
                for (byte in byteArrayOf(0, 0, 0, 3, 1)) input.writeByte(byte)
                input.flushAndClose()
                DaemonIpc.readFrame(input)
            }
        }
    }

    @Test
    fun readFrameRejectsInvalidLength() {
        assertThrows(IOException::class.java) {
            runBlocking {
                val input = ByteChannel()
                input.writeInt(0)
                input.flushAndClose()
                DaemonIpc.readFrame(input)
            }
        }
        assertThrows(IOException::class.java) {
            runBlocking {
                val input = ByteChannel()
                input.writeInt(DaemonIpc.MAX_FRAME_SIZE + 1)
                input.flushAndClose()
                DaemonIpc.readFrame(input)
            }
        }
    }

    @Test
    fun writeFrameRejectsInvalidLength() {
        assertThrows(IllegalArgumentException::class.java) {
            runBlocking { DaemonIpc.writeFrame(ByteChannel(), ByteArray(0)) }
        }
        assertThrows(IllegalArgumentException::class.java) {
            runBlocking { DaemonIpc.writeFrame(ByteChannel(), ByteArray(DaemonIpc.MAX_FRAME_SIZE + 1)) }
        }
    }
}
