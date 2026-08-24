package be.mygod.vpnhotspot.root.daemon

import io.ktor.utils.io.ByteReadChannel
import io.ktor.utils.io.ByteWriteChannel
import io.ktor.utils.io.readFully
import io.ktor.utils.io.readInt
import io.ktor.utils.io.writeFully
import io.ktor.utils.io.writeInt
import java.io.IOException

object DaemonIpc {
    /**
     * Matches Android's documented Binder transaction buffer size. AOSP's hidden
     * `IBinder.MAX_IPC_SIZE` is only the smaller suggested per-call cap.
     */
    const val MAX_FRAME_SIZE = 1024 * 1024

    suspend fun readFrame(input: ByteReadChannel): ByteArray {
        val length = input.readInt()
        if (length !in 1..MAX_FRAME_SIZE) throw IOException("Invalid daemon frame length $length")
        return ByteArray(length).also { input.readFully(it) }
    }

    suspend fun writeFrame(output: ByteWriteChannel, packet: ByteArray) {
        val length = packet.size
        require(length in 1..MAX_FRAME_SIZE) { "Invalid daemon frame length $length" }
        output.writeInt(length)
        output.writeFully(packet)
        output.flush()
    }
}
