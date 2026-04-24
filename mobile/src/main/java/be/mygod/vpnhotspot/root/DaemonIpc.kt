package be.mygod.vpnhotspot.root

import java.io.EOFException
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream

internal object DaemonIpc {
    const val STARTUP_TIMEOUT_MILLIS = 10_000L
    const val MAX_FRAME_SIZE = 65535

    fun readFrame(input: InputStream): ByteArray {
        val header = ByteArray(4)
        readFully(input, header)
        val length = ((header[0].toInt() and 0xFF) shl 24) or
                ((header[1].toInt() and 0xFF) shl 16) or
                ((header[2].toInt() and 0xFF) shl 8) or
                (header[3].toInt() and 0xFF)
        if (length <= 0 || length > MAX_FRAME_SIZE) throw IOException("Invalid daemon frame length $length")
        return ByteArray(length).also { readFully(input, it) }
    }

    fun writeFrame(output: OutputStream, packet: ByteArray) {
        val length = packet.size
        require(length in 1..MAX_FRAME_SIZE) { "Invalid daemon frame length $length" }
        output.write(byteArrayOf(
            (length ushr 24).toByte(),
            (length ushr 16).toByte(),
            (length ushr 8).toByte(),
            length.toByte(),
        ))
        output.write(packet)
    }

    private fun readFully(input: InputStream, buffer: ByteArray) {
        var offset = 0
        while (offset < buffer.size) {
            val count = input.read(buffer, offset, buffer.size - offset)
            if (count < 0) throw EOFException("daemon disconnected")
            offset += count
        }
    }
}
