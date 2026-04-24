package be.mygod.vpnhotspot.root

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertThrows
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.EOFException
import java.io.IOException
import java.io.InputStream

class DaemonIpcTest {
    @Test
    fun readFrameHandlesSplitHeaderAndBody() {
        assertArrayEquals(byteArrayOf(1, 2, 3),
            DaemonIpc.readFrame(ChunkedInputStream(byteArrayOf(0, 0, 0, 3, 1, 2, 3), 1)))
    }

    @Test
    fun readFrameHandlesMultipleFrames() {
        val output = ByteArrayOutputStream()
        DaemonIpc.writeFrame(output, byteArrayOf(1, 2))
        DaemonIpc.writeFrame(output, byteArrayOf(3))
        val input = ByteArrayInputStream(output.toByteArray())
        assertArrayEquals(byteArrayOf(1, 2), DaemonIpc.readFrame(input))
        assertArrayEquals(byteArrayOf(3), DaemonIpc.readFrame(input))
    }

    @Test
    fun readFrameRejectsUnexpectedEof() {
        assertThrows(EOFException::class.java) {
            DaemonIpc.readFrame(ByteArrayInputStream(byteArrayOf(0, 0, 0, 3, 1)))
        }
    }

    @Test
    fun readFrameRejectsInvalidLength() {
        assertThrows(IOException::class.java) {
            DaemonIpc.readFrame(ByteArrayInputStream(byteArrayOf(0, 0, 0, 0)))
        }
        assertThrows(IOException::class.java) {
            DaemonIpc.readFrame(ByteArrayInputStream(byteArrayOf(0, 1, 0, 0)))
        }
    }

    @Test
    fun writeFrameRejectsInvalidLength() {
        assertThrows(IllegalArgumentException::class.java) {
            DaemonIpc.writeFrame(ByteArrayOutputStream(), ByteArray(0))
        }
        assertThrows(IllegalArgumentException::class.java) {
            DaemonIpc.writeFrame(ByteArrayOutputStream(), ByteArray(DaemonIpc.MAX_FRAME_SIZE + 1))
        }
    }

    private class ChunkedInputStream(
        private val data: ByteArray,
        private val chunkSize: Int,
    ) : InputStream() {
        private var offset = 0

        override fun read(): Int {
            if (offset >= data.size) return -1
            return data[offset++].toInt() and 0xFF
        }

        override fun read(buffer: ByteArray, off: Int, len: Int): Int {
            if (offset >= data.size) return -1
            val count = minOf(len, chunkSize, data.size - offset)
            data.copyInto(buffer, off, offset, offset + count)
            offset += count
            return count
        }
    }
}
