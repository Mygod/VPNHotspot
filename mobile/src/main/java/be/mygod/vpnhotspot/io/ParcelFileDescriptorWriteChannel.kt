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
import io.ktor.utils.io.readAvailable
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.async
import kotlinx.coroutines.suspendCancellableCoroutine
import java.io.IOException
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.coroutines.resume

/**
 * Write channel backed by an owned [ParcelFileDescriptor] registered on a [MessageQueue].
 */
class ParcelFileDescriptorWriteChannel(
    private val descriptor: ParcelFileDescriptor,
    scope: CoroutineScope,
    looper: Looper = Looper.getMainLooper(),
    private val buffer: ByteArray = ByteArray(4096),
    private val channel: ByteChannel = ByteChannel(autoFlush = true),
) : ByteWriteChannel by channel {
    private val fileDescriptor = descriptor.fileDescriptor
    private val handler = Handler(looper)
    private val closed = AtomicBoolean()
    private val pump: Deferred<Unit>

    init {
        fileDescriptor.isNonblocking = true
        pump = scope.async {
            var failure: Throwable? = null
            var start = 0
            var end = 0
            try {
                while (true) {
                    if (start == end) {
                        val count = channel.readAvailable(buffer)
                        if (count < 0) break
                        start = 0
                        end = count
                    }
                    while (start < end) {
                        val count = try {
                            Os.write(fileDescriptor, buffer, start, end - start)
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
                        if (count > 0) {
                            start += count
                        } else {
                            awaitOutputReady()
                        }
                    }
                }
            } catch (e: Throwable) {
                failure = e
                channel.cancel(e)
                throw e
            } finally {
                val closeError = closeDescriptor()
                if (closeError != null) {
                    if (failure == null) {
                        channel.cancel(closeError)
                        throw closeError
                    } else {
                        failure.addSuppressed(closeError)
                    }
                }
            }
        }
    }

    override fun cancel(cause: Throwable?) {
        pump.cancel()
        val closeError = closeDescriptor()
        if (cause != null && closeError != null) cause.addSuppressed(closeError)
        channel.cancel(cause ?: closeError)
    }

    override suspend fun flushAndClose() {
        var failure: Throwable? = null
        try {
            channel.flushAndClose()
        } catch (e: Throwable) {
            failure = e
        }
        try {
            pump.await()
        } catch (e: Throwable) {
            if (failure == null) {
                failure = e
            } else if (failure !== e) {
                failure.addSuppressed(e)
            }
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
    scope: CoroutineScope,
    looper: Looper = Looper.getMainLooper(),
    buffer: ByteArray = ByteArray(4096),
) = ParcelFileDescriptorWriteChannel(this, scope, looper, buffer)
