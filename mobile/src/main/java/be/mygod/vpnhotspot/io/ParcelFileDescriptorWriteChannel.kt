package be.mygod.vpnhotspot.io

import android.os.Handler
import android.os.Looper
import android.os.MessageQueue
import android.os.ParcelFileDescriptor
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import io.ktor.utils.io.ByteChannel
import io.ktor.utils.io.ByteWriteChannel
import io.ktor.utils.io.availableForRead
import io.ktor.utils.io.readAvailable
import kotlinx.coroutines.suspendCancellableCoroutine
import java.io.IOException
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.coroutines.resume

/**
 * Write channel backed by an owned [ParcelFileDescriptor] registered on a [MessageQueue].
 */
class ParcelFileDescriptorWriteChannel(
    private val descriptor: ParcelFileDescriptor,
    looper: Looper = Looper.getMainLooper(),
    private val buffer: ByteArray = ByteArray(4096),
    private val channel: ByteChannel = ByteChannel(),
) : ByteWriteChannel by channel {
    private val fileDescriptor = descriptor.fileDescriptor
    private val handler = Handler(looper)
    private val closed = AtomicBoolean()

    init {
        fileDescriptor.isNonblocking = true
    }

    override fun cancel(cause: Throwable?) {
        val closeError = closeDescriptor()
        if (cause != null && closeError != null) cause.addSuppressed(closeError)
        channel.cancel(cause ?: closeError)
    }

    override suspend fun flush() {
        channel.flush()
        while (channel.availableForRead > 0) {
            val count = channel.readAvailable(buffer)
            if (count < 0) break
            var offset = 0
            while (offset < count) {
                val written = try {
                    Os.write(fileDescriptor, buffer, offset, count - offset)
                } catch (e: ErrnoException) {
                    when (e.errno) {
                        OsConstants.EAGAIN -> {
                            awaitOutputReady()
                            continue
                        }
                        OsConstants.EINTR -> continue
                        else -> throw e
                    }
                }
                if (written > 0) {
                    offset += written
                } else {
                    awaitOutputReady()
                }
            }
        }
    }

    override suspend fun flushAndClose() {
        var failure: Throwable? = null
        try {
            channel.flushAndClose()
            flush()
        } catch (e: Throwable) {
            failure = e
        }
        closeDescriptor()?.let {
            if (failure == null) {
                failure = it
            } else {
                failure.addSuppressed(it)
            }
        }
        failure?.let { throw it }
    }

    private suspend fun awaitOutputReady() {
        val events = MessageQueue.OnFileDescriptorEventListener.EVENT_OUTPUT or
                MessageQueue.OnFileDescriptorEventListener.EVENT_ERROR
        val messageQueue = handler.looper.queue
        var listener: MessageQueue.OnFileDescriptorEventListener? = null
        try {
            suspendCancellableCoroutine { continuation ->
                listener = MessageQueue.OnFileDescriptorEventListener { _, _ ->
                    handler.post {
                        if (continuation.isActive) continuation.resume(Unit)
                    }
                    0
                }
                messageQueue.addOnFileDescriptorEventListener(fileDescriptor, events, listener)
                continuation.invokeOnCancellation {
                    messageQueue.removeOnFileDescriptorEventListener(fileDescriptor)
                }
            }
        } finally {
            if (listener != null) messageQueue.removeOnFileDescriptorEventListener(fileDescriptor)
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

fun ParcelFileDescriptor.openWriteChannel(
    looper: Looper = Looper.getMainLooper(),
    buffer: ByteArray = ByteArray(4096),
) = ParcelFileDescriptorWriteChannel(this, looper, buffer)
