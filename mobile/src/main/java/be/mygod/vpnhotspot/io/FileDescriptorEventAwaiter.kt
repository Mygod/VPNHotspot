package be.mygod.vpnhotspot.io

import android.os.Handler
import android.os.MessageQueue
import kotlinx.coroutines.CancellableContinuation
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.suspendCancellableCoroutine
import java.io.FileDescriptor
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference
import kotlin.coroutines.resume

/**
 * One-shot readiness awaiter backed by a persistent MessageQueue fd record when possible.
 */
internal class FileDescriptorEventAwaiter(
    private val fileDescriptor: FileDescriptor,
    private val handler: Handler,
) : MessageQueue.OnFileDescriptorEventListener {
    private val messageQueue = handler.looper.queue
    private val continuation = AtomicReference<CancellableContinuation<Unit>?>()
    private val closed = AtomicBoolean()

    suspend fun await(events: Int) {
        if (closed.get()) throw CancellationException("File descriptor listener closed")
        suspendCancellableCoroutine { continuation ->
            if (closed.get()) {
                continuation.cancel(CancellationException("File descriptor listener closed"))
                return@suspendCancellableCoroutine
            }
            check(this.continuation.compareAndSet(null, continuation)) {
                "Already waiting for file descriptor readiness"
            }
            continuation.invokeOnCancellation {
                if (this.continuation.compareAndSet(continuation, null) && !closed.get()) {
                    messageQueue.addOnFileDescriptorEventListener(
                            fileDescriptor,
                            MessageQueue.OnFileDescriptorEventListener.EVENT_ERROR,
                            this)
                }
            }
            try {
                if (continuation.isActive) {
                    if (closed.get()) {
                        if (this.continuation.compareAndSet(continuation, null)) {
                            continuation.cancel(CancellationException("File descriptor listener closed"))
                        }
                    } else {
                        messageQueue.addOnFileDescriptorEventListener(
                                fileDescriptor,
                                events or MessageQueue.OnFileDescriptorEventListener.EVENT_ERROR,
                                this)
                    }
                }
            } catch (e: Throwable) {
                this.continuation.compareAndSet(continuation, null)
                throw e
            }
        }
    }

    override fun onFileDescriptorEvents(fd: FileDescriptor, events: Int): Int {
        val continuation = continuation.getAndSet(null) ?: return 0
        handler.post {
            if (continuation.isActive) continuation.resume(Unit)
        }
        return MessageQueue.OnFileDescriptorEventListener.EVENT_ERROR
    }

    fun close() {
        if (!closed.compareAndSet(false, true)) return
        continuation.getAndSet(null)?.cancel(CancellationException("File descriptor listener closed"))
        messageQueue.removeOnFileDescriptorEventListener(fileDescriptor)
    }
}
