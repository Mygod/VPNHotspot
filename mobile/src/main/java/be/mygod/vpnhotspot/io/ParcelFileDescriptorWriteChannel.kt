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
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.android.asCoroutineDispatcher
import kotlinx.coroutines.launch
import java.io.IOException
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Write channel backed by an owned [ParcelFileDescriptor] registered on a [MessageQueue].
 */
internal class ParcelFileDescriptorWriteChannel(
    private val descriptor: ParcelFileDescriptor,
    looper: Looper = Looper.getMainLooper(),
    private val buffer: ByteArray = ByteArray(4096),
    private val channel: ByteChannel = ByteChannel(),
) : ByteWriteChannel by channel {
    private val fileDescriptor = descriptor.fileDescriptor
    private val handler = Handler(looper)
    private val eventAwaiter = FileDescriptorEventAwaiter(fileDescriptor, handler)
    private val closed = AtomicBoolean()
    @Volatile
    private var drainFailure: Throwable? = null
    private val drainJob: Job

    init {
        fileDescriptor.isNonblocking = true
        drainJob = CoroutineScope(handler.asCoroutineDispatcher("pfd-writer")).launch {
            try {
                while (true) {
                    val count = channel.readAvailable(buffer)
                    if (count < 0) break
                    var offset = 0
                    while (offset < count) {
                        val written = try {
                            Os.write(fileDescriptor, buffer, offset, count - offset)
                        } catch (e: ErrnoException) {
                            when (e.errno) {
                                OsConstants.EAGAIN -> {
                                    eventAwaiter.await(MessageQueue.OnFileDescriptorEventListener.EVENT_OUTPUT)
                                    continue
                                }
                                OsConstants.EINTR -> continue
                                else -> throw e
                            }
                        }
                        if (written > 0) {
                            offset += written
                        } else {
                            eventAwaiter.await(MessageQueue.OnFileDescriptorEventListener.EVENT_OUTPUT)
                        }
                    }
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Throwable) {
                drainFailure = e
                channel.cancel(e)
            } finally {
                closeDescriptor()?.let { closeError ->
                    drainFailure = drainFailure?.also { it.addSuppressed(closeError) } ?: closeError
                }
            }
        }
    }

    override fun cancel(cause: Throwable?) {
        val closeError = closeDescriptor()
        if (cause != null && closeError != null) cause.addSuppressed(closeError)
        drainFailure = cause ?: closeError
        channel.cancel(drainFailure)
        drainJob.cancel()
    }

    override suspend fun flushAndClose() {
        try {
            channel.flushAndClose()
        } catch (e: Throwable) {
            drainFailure = drainFailure?.also { it.addSuppressed(e) } ?: e
        }
        drainJob.join()
        drainFailure?.let { throw it }
    }

    private fun closeDescriptor(): IOException? {
        if (!closed.compareAndSet(false, true)) return null
        eventAwaiter.close()
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
): ByteWriteChannel = ParcelFileDescriptorWriteChannel(this, looper, buffer)
