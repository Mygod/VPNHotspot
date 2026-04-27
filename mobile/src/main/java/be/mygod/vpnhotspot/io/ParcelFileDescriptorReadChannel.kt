package be.mygod.vpnhotspot.io

import android.os.Looper
import android.os.MessageQueue
import android.os.ParcelFileDescriptor
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import io.ktor.utils.io.ByteChannel
import io.ktor.utils.io.ByteReadChannel
import io.ktor.utils.io.InternalAPI
import java.io.IOException
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Read channel backed by an owned [ParcelFileDescriptor] registered on a [MessageQueue].
 */
@OptIn(InternalAPI::class)
class ParcelFileDescriptorReadChannel(
    private val descriptor: ParcelFileDescriptor,
    looper: Looper = Looper.getMainLooper(),
    private val buffer: ByteArray = ByteArray(4096),
    private val channel: ByteChannel = ByteChannel(autoFlush = true),
) : ByteReadChannel by channel {
    private val fileDescriptor = descriptor.fileDescriptor
    private val messageQueue = looper.queue
    private val events = MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT or
            MessageQueue.OnFileDescriptorEventListener.EVENT_ERROR
    private val active = AtomicBoolean(true)
    private val closed = AtomicBoolean()
    private val listener = MessageQueue.OnFileDescriptorEventListener { _, _ -> drain() }

    init {
        fileDescriptor.isNonblocking = true
        messageQueue.addOnFileDescriptorEventListener(fileDescriptor, events, listener)
        if (drain() == 0) messageQueue.removeOnFileDescriptorEventListener(fileDescriptor)
    }

    /**
     * Drain currently readable descriptor bytes into this channel without waiting.
     */
    fun drain(): Int {
        if (!active.get() || channel.isClosedForWrite) return 0
        var result = events
        while (result == events) {
            try {
                val count = try {
                    Os.read(fileDescriptor, buffer, 0, buffer.size)
                } catch (e: ErrnoException) {
                    when (e.errno) {
                        OsConstants.EAGAIN -> break
                        OsConstants.EINTR -> continue
                        OsConstants.EBADF -> {
                            complete()
                            result = 0
                            break
                        }
                        else -> throw e
                    }
                }
                if (count == 0) {
                    complete()
                    result = 0
                    break
                }
                channel.writeBuffer.write(buffer, 0, count)
                channel.flushWriteBuffer()
            } catch (e: Throwable) {
                complete(e)
                result = 0
            }
        }
        return result
    }

    override fun cancel(cause: Throwable?) = complete(cause)

    private fun complete(cause: Throwable? = null) {
        if (!active.compareAndSet(true, false)) return
        messageQueue.removeOnFileDescriptorEventListener(fileDescriptor)
        val closeError = closeDescriptor()
        if (cause == null) {
            if (closeError == null) {
                if (!channel.isClosedForWrite) channel.close()
            } else {
                channel.cancel(closeError)
            }
        } else {
            if (closeError != null) cause.addSuppressed(closeError)
            channel.cancel(cause)
        }
    }

    private fun closeDescriptor(): IOException? {
        if (!closed.compareAndSet(false, true)) return null
        return try {
            descriptor.close()
            null
        } catch (e: IOException) {
            e
        }
    }
}

fun ParcelFileDescriptor.openReadChannel(
    looper: Looper = Looper.getMainLooper(),
    buffer: ByteArray = ByteArray(4096),
) = ParcelFileDescriptorReadChannel(this, looper, buffer)
