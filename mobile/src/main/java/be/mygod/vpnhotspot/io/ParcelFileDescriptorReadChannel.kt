package be.mygod.vpnhotspot.io

import android.os.Handler
import android.os.Looper
import android.os.MessageQueue
import android.os.ParcelFileDescriptor
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import io.ktor.utils.io.ByteChannel
import io.ktor.utils.io.ByteReadChannel
import io.ktor.utils.io.writeFully
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.android.asCoroutineDispatcher
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import java.io.IOException
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Read channel backed by an owned [ParcelFileDescriptor] registered on a [MessageQueue].
 */
internal class ParcelFileDescriptorReadChannel(
    private val descriptor: ParcelFileDescriptor,
    looper: Looper = Looper.getMainLooper(),
    private val buffer: ByteArray = ByteArray(4096),
    private val channel: ByteChannel = ByteChannel(autoFlush = true),
) : ByteReadChannel by channel {
    private val fileDescriptor = descriptor.fileDescriptor
    private val handler = Handler(looper)
    private val eventAwaiter = FileDescriptorEventAwaiter(fileDescriptor, handler)
    private val closed = AtomicBoolean()
    private val drainLock = Mutex()
    @Volatile
    private var drainFailure: Throwable? = null
    private val drainJob: Job

    init {
        fileDescriptor.isNonblocking = true
        drainJob = CoroutineScope(handler.asCoroutineDispatcher("pfd-reader")).launch {
            try {
                while (drainAvailable()) {
                    eventAwaiter.await(MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT)
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Throwable) {
                complete(e)
            }
        }
    }

    /**
     * Drain currently readable descriptor bytes into this channel without waiting for more fd input.
     */
    suspend fun drain() {
        drainFailure?.let { throw it }
        try {
            if (!drainAvailable()) drainJob.cancel()
        } catch (e: CancellationException) {
            throw e
        } catch (e: Throwable) {
            complete(e)
            throw e
        }
        drainFailure?.let { throw it }
    }

    override fun cancel(cause: Throwable?) {
        complete(cause)
        drainJob.cancel()
    }

    /**
     * @return true if reading stopped because the descriptor would block, false after EOF/close.
     */
    private suspend fun drainAvailable(): Boolean = drainLock.withLock {
        while (!channel.isClosedForWrite) {
            val count = try {
                Os.read(fileDescriptor, buffer, 0, buffer.size)
            } catch (e: ErrnoException) {
                when (e.errno) {
                    OsConstants.EAGAIN -> return true
                    OsConstants.EINTR -> continue
                    OsConstants.EBADF -> {
                        complete()
                        return false
                    }
                    else -> throw e
                }
            }
            if (count == 0) {
                complete()
                return false
            }
            channel.writeFully(buffer, 0, count)
        }
        false
    }

    private fun complete(cause: Throwable? = null) {
        val closeError = if (!closed.compareAndSet(false, true)) null else {
            eventAwaiter.close()
            try {
                descriptor.close()
                null
            } catch (e: IOException) {
                e
            }
        }
        if (cause == null) {
            if (closeError == null) {
                if (!channel.isClosedForWrite) channel.close()
            } else {
                drainFailure = closeError
                channel.cancel(closeError)
            }
        } else {
            if (closeError != null) cause.addSuppressed(closeError)
            drainFailure = cause
            channel.cancel(cause)
        }
    }
}

fun ParcelFileDescriptor.openReadChannel(
    looper: Looper = Looper.getMainLooper(),
    buffer: ByteArray = ByteArray(4096),
): ByteReadChannel = ParcelFileDescriptorReadChannel(this, looper, buffer)
