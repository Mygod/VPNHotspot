package be.mygod.vpnhotspot.shizuku

import android.net.LocalServerSocket
import android.net.LocalSocket
import android.os.ParcelFileDescriptor
import android.os.Process
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.io.awaitExit
import be.mygod.librootkotlinx.io.pid
import be.mygod.librootkotlinx.net.ALocalServerSocket
import be.mygod.librootkotlinx.net.ALocalSocket
import be.mygod.vpnhotspot.root.daemon.CancelCommand
import be.mygod.vpnhotspot.root.daemon.ClientEnvelope
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonEnvelope
import be.mygod.vpnhotspot.root.daemon.DaemonException
import be.mygod.vpnhotspot.root.daemon.DaemonIpc
import be.mygod.vpnhotspot.root.daemon.ShizukuSessionConfig
import be.mygod.vpnhotspot.root.daemon.StartShizukuSessionCommand
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.widget.SmartSnackbar
import io.ktor.utils.io.ByteReadChannel
import io.ktor.utils.io.ByteWriteChannel
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.job
import kotlinx.coroutines.launch
import kotlinx.coroutines.selects.select
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import timber.log.Timber
import java.io.EOFException
import java.io.IOException
import java.lang.Process as ChildProcess
import java.nio.ByteBuffer
import kotlin.random.Random
import kotlin.time.Duration.Companion.seconds

/**
 * One app-UID daemon process and one call-multiplexed control conversation for a Shizuku session.
 * Config calls and receive share [privilegedDispatcher]; each acknowledgement is installed before its
 * suspending frame write so the reader cannot overtake it.
 */
@RequiresApi(30)
class AppUidDaemon private constructor(
    private val input: ByteReadChannel,
    private val output: ByteWriteChannel,
) {
    class Child internal constructor(
        private val process: ChildProcess,
        private val pid: Int,
        private val server: ALocalServerSocket,
        private val scope: CoroutineScope,
    ) {
        private var socket: ALocalSocket? = null

        private val output = StringBuilder()
        @Volatile
        private var capturing = true

        private val draining = scope.launch(Dispatchers.IO) {
            try {
                process.inputStream.bufferedReader().forEachLine {
                    Timber.tag(BINARY_NAME).i(it)
                    if (capturing) output.appendLine(it)
                }
            } catch (e: IOException) {
                Timber.tag(BINARY_NAME).d(e)
            }
        }

        internal val exited = scope.async {
            val status = process.awaitExit()
            draining.join()
            IOException(buildString {
                append(BINARY_NAME).append(' ').append(pid).append(" exited with ").append(status)
                if (output.isEmpty()) append(" without printing anything") else {
                    append(" after printing: ").append(output)
                }
            })
        }

        /** Closes the control socket, escalates EOF to signals if needed, and returns only after observed exit. */
        suspend fun stop() = withContext(NonCancellable) {
            for (closeable in arrayOf(socket, server)) try {
                closeable?.close()
            } catch (e: IOException) {
                Timber.w(e)
            }
            if (!awaitExit(GRACEFUL_EXIT_SECONDS)) {
                process.destroy()
                if (!awaitExit(SIGTERM_EXIT_SECONDS)) {
                    try {
                        Os.kill(pid, OsConstants.SIGKILL)
                    } catch (e: ErrnoException) {
                        if (e.errno != OsConstants.ESRCH) throw e
                    }
                    check(awaitExit(SIGKILL_EXIT_SECONDS)) { "$BINARY_NAME $pid outlived SIGKILL" }
                }
            }
            scope.coroutineContext.job.cancelAndJoin()
            Timber.i("$BINARY_NAME $pid exited with ${process.exitValue()}")
        }

        private suspend fun awaitExit(seconds: Long) =
            withTimeoutOrNull(seconds.seconds) { process.awaitExit() } != null

        /** Authenticates the launched PID and app UID before transferring the TUN descriptor. */
        internal suspend fun connect(
            tun: ParcelFileDescriptor,
            command: StartShizukuSessionCommand,
        ): AppUidDaemon {
            Os.fcntlInt(tun.fileDescriptor, OsConstants.F_SETFL,
                Os.fcntlInt(tun.fileDescriptor, OsConstants.F_GETFL, 0) or OsConstants.O_NONBLOCK)
            while (true) {
                val accepting = scope.async { server.accept() }
                val accepted = try {
                    select<ALocalSocket> {
                        accepting.onAwait { it }
                        exited.onAwait { throw it }
                    }
                } catch (e: Throwable) {
                    try {
                        server.close()
                    } catch (closing: IOException) {
                        e.addSuppressed(closing)
                    }
                    withContext(NonCancellable) {
                        val stranded = try {
                            accepting.await()
                        } catch (loser: Throwable) {
                            if (loser !== e && loser !is CancellationException && loser !is IOException) {
                                e.addSuppressed(loser)
                            }
                            null
                        }
                        if (stranded != null) try {
                            stranded.close()
                        } catch (closing: IOException) {
                            e.addSuppressed(closing)
                        }
                    }
                    throw e
                }
                var keep = false
                try {
                    val credentials = accepted.socket.peerCredentials
                    if (credentials.uid != Process.myUid() || credentials.pid != pid) {
                        Timber.w("Rejected $BINARY_NAME connection from uid=${credentials.uid} " +
                                "pid=${credentials.pid}, expected uid=${Process.myUid()} pid=$pid")
                        continue
                    }
                    val input = accepted.openReadChannel()
                    socket = accepted
                    keep = true
                    tun.dup().use { duplicate ->
                        accepted.socket.setFileDescriptorsForSend(arrayOf(duplicate.fileDescriptor))
                        try {
                            writeFrameWithDescriptor(accepted.socket, ClientEnvelope.ADAPTER.encode(
                                ClientEnvelope(call_id = SESSION_CALL_ID, start_shizuku_session = command)))
                        } finally {
                            accepted.socket.setFileDescriptorsForSend(null)
                        }
                    }
                    val daemon = create(input, accepted.openWriteChannel(), scope)
                    select<Unit> {
                        daemon.started.onAwait { }
                        daemon.ended.onAwait { cause ->
                            throw cause ?: exited.await()
                        }
                    }
                    Timber.i("$BINARY_NAME ${credentials.pid} is ready on ${command.interface_name}")
                    capturing = false
                    server.close()
                    return daemon
                } finally {
                    if (!keep) try {
                        accepted.close()
                    } catch (e: IOException) {
                        Timber.w(e)
                    }
                }
            }
        }
    }
    private var pending: Boolean? = null
    private var applying = false

    /** A partial frame poisons future writes; only the reader decides the terminal conversation cause. */
    private var poisoned: Throwable? = null

    /** Coalesces to the latest admission state and waits for its ACK. */
    suspend fun apply(admit: Boolean) {
        pending = admit
        if (applying) return
        applying = true
        try {
            while (true) {
                poisoned?.let { throw it }
                if (ended.isCompleted) {
                    throw ended.await() ?: IOException("$BINARY_NAME is no longer answering")
                }
                val next = pending ?: return
                pending = null
                val id = nextCallId++
                val reply = CompletableDeferred<Unit>()
                val acknowledgement = Acknowledgement(id, reply)
                this.acknowledgement = acknowledgement
                try {
                    DaemonIpc.writeFrame(output, ClientEnvelope.ADAPTER.encode(ClientEnvelope(
                        call_id = id,
                        apply_shizuku_config = ShizukuSessionConfig(admit = next))))
                } catch (e: Exception) {
                    if (this.acknowledgement === acknowledgement) {
                        this.acknowledgement = null
                        reply.completeExceptionally(e)
                    }
                    if (poisoned == null) poisoned = if (e is CancellationException) {
                        IOException("$BINARY_NAME config call $id was interrupted mid-frame", e)
                    } else e
                    if (e !is CancellationException) ended.await()?.let { throw it }
                    throw e
                }
                try {
                    select<Unit> {
                        reply.onAwait { }
                        ended.onAwait { cause ->
                            throw cause ?: IOException(
                                "$BINARY_NAME stopped answering config call $id")
                        }
                    }
                } catch (e: CancellationException) {
                    withContext(NonCancellable) { abandon(id) }
                    throw e
                }
            }
        } finally {
            applying = false
        }
    }

    private suspend fun abandon(id: Long) {
        if (acknowledgement?.id == id) acknowledgement = null
        try {
            DaemonIpc.writeFrame(output, ClientEnvelope.ADAPTER.encode(
                ClientEnvelope(call_id = id, cancel = CancelCommand())))
        } catch (e: Exception) {
            if (poisoned == null) poisoned = if (e is CancellationException) {
                IOException("$BINARY_NAME call $id could not be cancelled", e)
            } else e
            Timber.tag(BINARY_NAME).d(e)
        }
    }

    private var nextCallId = SESSION_CALL_ID + 1

    private class Acknowledgement(val id: Long, val reply: CompletableDeferred<Unit>)

    private var acknowledgement: Acknowledgement? = null

    private val started = CompletableDeferred<Unit>()

    /** Reader-owned terminal cause; null means clean completion or unexplained EOF. */
    val ended = CompletableDeferred<Throwable?>()

    private suspend fun receive() {
        var cause: Throwable? = null
        try {
            while (true) {
                val packet = try {
                    DaemonIpc.readFrame(input)
                } catch (e: EOFException) {
                    Timber.tag(BINARY_NAME).d(e)
                    return
                }
                val envelope = DaemonEnvelope.ADAPTER.decode(packet)
                when {
                    envelope.non_fatal != null -> {
                        val frame = envelope.non_fatal
                        val traced = DaemonException(
                            frame.report ?: throw IOException("Missing daemon nonfatal report"),
                            frame.call_id?.readCallId(), BINARY_NAME).withCurrentTrace()
                        Timber.tag(BINARY_NAME).w(traced)
                        SmartSnackbar.make(traced).show()
                    }
                    envelope.error != null -> {
                        val frame = envelope.error
                        val id = frame.call_id.readCallId()
                        val exception = DaemonException(
                            frame.report ?: throw IOException("Missing daemon error report"),
                            id, BINARY_NAME)
                        cause = exception
                        if (id != SESSION_CALL_ID) takeAcknowledgement(id)?.completeExceptionally(exception)
                        return
                    }
                    envelope.reply != null -> {
                        val frame = envelope.reply
                        val id = frame.call_id.readCallId()
                        frame.ack
                            ?: throw IOException("$BINARY_NAME answered a config call with $frame")
                        takeAcknowledgement(id)?.complete(Unit)
                    }
                    envelope.event != null -> {
                        val frame = envelope.event
                        if (frame.call_id.readCallId() != SESSION_CALL_ID || frame.ack == null) {
                            throw IOException("$BINARY_NAME sent an unexpected event $frame")
                        }
                        if (!started.complete(Unit)) {
                            throw IOException("$BINARY_NAME acknowledged the session twice")
                        }
                    }
                    envelope.complete != null -> {
                        if (envelope.complete.call_id.readCallId() != SESSION_CALL_ID) {
                            throw IOException("$BINARY_NAME completed call " +
                                    "${envelope.complete.call_id}, which is not the session")
                        }
                        if (!started.isCompleted) {
                            throw IOException("$BINARY_NAME completed the session before starting it")
                        }
                        return
                    }
                    else -> throw IOException("$BINARY_NAME sent an empty frame")
                }
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Timber.tag(BINARY_NAME).w(e)
            cause = e
        } finally {
            acknowledgement?.reply?.completeExceptionally(cause
                ?: IOException("$BINARY_NAME stopped answering"))
            acknowledgement = null
            ended.complete(cause)
        }
    }

    private fun Long.readCallId(): Long {
        if (this <= 0) throw IOException("Invalid $BINARY_NAME call id $this")
        return this
    }

    private fun takeAcknowledgement(id: Long): CompletableDeferred<Unit>? {
        val acknowledgement = acknowledgement
        if (acknowledgement?.id != id) {
            Timber.tag(BINARY_NAME).w("Dropping an answer for $BINARY_NAME call $id")
            return null
        }
        this.acknowledgement = null
        return acknowledgement.reply
    }

    companion object {
        private const val BINARY_NAME = "vpnhotspotd"

        private const val SESSION_CALL_ID = 1L

        private const val GRACEFUL_EXIT_SECONDS = 10L
        private const val SIGTERM_EXIT_SECONDS = 5L
        private const val SIGKILL_EXIT_SECONDS = 5L

        /** A single write attaches the descriptor once rather than once per frame fragment. */
        private suspend fun writeFrameWithDescriptor(socket: LocalSocket, packet: ByteArray) {
            require(packet.size in 1..DaemonIpc.MAX_FRAME_SIZE) {
                "Invalid daemon frame length ${packet.size}"
            }
            val frame = ByteBuffer.allocate(Int.SIZE_BYTES + packet.size).putInt(packet.size).put(packet)
            withContext(Dispatchers.IO) { socket.outputStream.write(frame.array()) }
        }

        /** Returns process ownership before the fallible authentication/start handshake. */
        fun spawn(): Child {
            val socketName = "$BINARY_NAME.${Process.myPid()}.${Random.nextLong().toHexString()}"
            val scope = CoroutineScope(privilegedDispatcher + SupervisorJob())
            val server = ALocalServerSocket(LocalServerSocket(socketName), Services.mainHandler)
            var process: ChildProcess? = null
            val pid = try {
                process = ProcessBuilder(DaemonController.daemonCommand + listOf("--app-uid", socketName))
                    .redirectErrorStream(true)
                    .start()
                process.pid.also { check(it > 0) { "$BINARY_NAME launched with pid $it" } }
            } catch (e: Throwable) {
                process?.destroy()
                scope.cancel()
                try {
                    server.close()
                } catch (closing: IOException) {
                    e.addSuppressed(closing)
                }
                throw e
            }
            return Child(process, pid, server, scope)
        }

        internal fun create(
            input: ByteReadChannel,
            output: ByteWriteChannel,
            scope: CoroutineScope,
        ) = AppUidDaemon(input, output).also { daemon ->
            scope.launch { daemon.receive() }
        }
    }
}
