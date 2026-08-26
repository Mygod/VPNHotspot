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
import be.mygod.vpnhotspot.root.daemon.BootstrapConfig
import be.mygod.vpnhotspot.root.daemon.BootstrapReady
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonException
import be.mygod.vpnhotspot.root.daemon.DaemonIpc
import be.mygod.vpnhotspot.root.daemon.ShizukuApplied
import be.mygod.vpnhotspot.root.daemon.ShizukuDaemonFrame
import be.mygod.vpnhotspot.root.daemon.ShizukuSessionConfig
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
import kotlinx.coroutines.cancel
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.job
import kotlinx.coroutines.launch
import kotlinx.coroutines.selects.select
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.withTimeoutOrNull
import timber.log.Timber
import java.io.IOException
import java.lang.Process as ChildProcess
import java.nio.ByteBuffer
import kotlin.random.Random
import kotlin.time.Duration.Companion.seconds

/**
 * The Shizuku-mode daemon, launched directly by the app process under the app UID. Not a Shizuku
 * UserService, not a root shell, not a persistent `app_process`, and not a JNI-hosted engine: the same
 * installed `vpnhotspotd` entry the root mode uses, exec'd in place from the APK.
 *
 * The privileged identity is deliberately absent here. Shell is barely better than the app UID for a
 * dataplane, since neither can touch netfilter, so nothing in this process needs Shizuku at all.
 */
@RequiresApi(33)
class AppUidDaemon private constructor(
    private val input: ByteReadChannel,
    private val output: ByteWriteChannel,
    /**
     * The session's own axes owner, which stamps the sequence as a config is written. Shared rather than
     * counted here, because the sequence is one of the three axes the daemon acknowledges together and
     * splitting it from the other two would leave no single place that decides what a published config says.
     */
    private val publication: SessionPublication,
) {
    /**
     * The launched child, owned from `ProcessBuilder.start()` rather than from a completed handshake.
     *
     * Ownership begins at launch because the process exists before it connects. A failure after the config
     * frame is written may also leave it holding a duplicate of the TUN. Once [spawn] returns, any later
     * failure leaves the caller's ledger to run the same exit fence as a normal stop before closing the TUN.
     */
    class Child internal constructor(
        private val process: ChildProcess,
        /**
         * Read from the launched process at `start()` and never from anything the peer says, because this is
         * what SIGKILL names. A child that never connects has no peer credentials at all, and one that
         * connects from another process is somebody else - so taking the pid from the connection would leave
         * the first case unfenceable and let the second redirect the fence at an innocent process.
         *
         * `java.lang.Process` has no `pid()` member on any supported release; the accessor is
         * [be.mygod.librootkotlinx.io.pid], which owns the `java.lang.UNIXProcess.pid` reflection, and its
         * availability is probed before the session mutates anything.
         */
        private val pid: Int,
        private val server: ALocalServerSocket,
        private val scope: CoroutineScope,
    ) {
        /** The accepted control socket, closed first because EOF on it is the child's cancellation signal. */
        private var socket: ALocalSocket? = null

        /**
         * Ordered shutdown, and a fence rather than a signal: everything downstream assumes the child is
         * gone, so this waits for observed exit.
         *
         * `destroyForcibly()` is not a force kill on Android - `UNIXProcess` does not override it, so it
         * calls `destroy()`, whose native side is `kill(pid, SIGTERM)`. Escalating to it would send SIGTERM
         * twice and do nothing for the wedged child the escalation exists to handle, so SIGKILL is sent
         * explicitly. The app may signal the child because they share a UID.
         *
         * https://android.googlesource.com/platform/libcore/+/refs/tags/android-17.0.0_r1/ojluni/src/main/native/UNIXProcess_md.c#1056
         */
        suspend fun stop() = withContext(NonCancellable) {
            // EOF on the control socket is the authoritative cancellation signal
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
                        // ESRCH means it exited between the check and the signal, which is the goal
                        if (e.errno != OsConstants.ESRCH) throw e
                    }
                    check(awaitExit(SIGKILL_EXIT_SECONDS)) { "$BINARY_NAME $pid outlived SIGKILL" }
                }
            }
            // Cancelled *and joined*, after observed exit: this scope owns the stdout drain and the control
            // reader, and both hold descriptors of a process everything downstream is about to treat as gone.
            // Cancellation is a request, and returning on the request rather than on its completion would let
            // the caller close the TUN and report the session gone while a reader was still running.
            scope.coroutineContext.job.cancelAndJoin()
            Timber.i("$BINARY_NAME $pid exited with ${process.exitValue()}")
        }

        private suspend fun awaitExit(seconds: Long) =
            withTimeoutOrNull(seconds.seconds) { process.awaitExit() } != null

        /**
         * Peer credentials are checked before a descriptor is handed over. The uid check rejects any other
         * app that guessed the abstract socket name, and the pid check rejects anything that is not the
         * process this launch created, including another copy of this app's own daemon.
         *
         * A peer that fails either check is dropped and the loop keeps accepting. Nothing it presented is
         * retained - above all not its pid, which stays the launched one, because a same-UID impostor that
         * could redirect the fence would be pointing SIGKILL at an innocent process while the real child kept
         * relaying.
         */
        internal suspend fun connect(
            tun: ParcelFileDescriptor,
            config: BootstrapConfig,
            publication: SessionPublication,
        ): AppUidDaemon {
            while (true) {
                val accepted = server.accept()
                var keep = false
                try {
                    val credentials = accepted.socket.peerCredentials
                    if (credentials.uid != Process.myUid() || credentials.pid != pid) {
                        Timber.w("Rejected $BINARY_NAME connection from uid=${credentials.uid} " +
                                "pid=${credentials.pid}, expected uid=${Process.myUid()} pid=$pid")
                        continue
                    }
                    // Reads go through the nonblocking channel, because the accepted socket is
                    // nonblocking and a plain read returns EAGAIN. The one write does not, because
                    // ancillary descriptors are state of LocalSocketImpl's output stream: a channel that
                    // writes the raw descriptor by another route silently drops them.
                    val input = accepted.openReadChannel()
                    // Retained only now, with both checks passed: this is the socket whose EOF is the
                    // child's cancellation signal, so it must not be one an impostor holds.
                    socket = accepted
                    keep = true
                    // attaches to the next write on this socket, which is the config frame below
                    tun.dup().use { duplicate ->
                        accepted.socket.setFileDescriptorsForSend(arrayOf(duplicate.fileDescriptor))
                        try {
                            writeFrameWithDescriptor(accepted.socket, BootstrapConfig.ADAPTER.encode(config))
                        } finally {
                            accepted.socket.setFileDescriptorsForSend(null)
                        }
                    }
                    val ready = BootstrapReady.ADAPTER.decode(DaemonIpc.readFrame(input))
                    if (ready.interface_name != config.interface_name) {
                        throw IOException("$BINARY_NAME reported interface ${ready.interface_name}")
                    }
                    Timber.i("$BINARY_NAME ${credentials.pid} is ready on ${config.interface_name}")
                    // Authentication is complete and this child is the only accepted peer.
                    server.close()
                    return AppUidDaemon(input, accepted.openWriteChannel(), publication).also { daemon ->
                        // Started only now, because the handshake above reads the same channel. From here
                        // nothing else does.
                        scope.launch { daemon.receive() }
                    }
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
    private var pending: ShizukuSessionConfig? = null
    private var applying = false

    /**
     * Sends one session config and waits for the daemon to acknowledge what it applied.
     *
     * Coalesced through a single pending slot rather than queued: the newest config is the whole truth, so
     * an older one has nothing left to say. The slot is necessary even though every caller runs on
     * [privilegedDispatcher], because this suspends while awaiting the acknowledgement and another
     * observation can reach the lane during that window - a single-threaded dispatcher orders dispatches,
     * not run-to-completion sections.
     *
     * A superseded caller returns normally instead of waiting for an acknowledgement that will never name
     * its config. The session was updated correctly; expiring it at the deadline would retire a healthy
     * dataplane.
     */
    suspend fun apply(config: ShizukuSessionConfig) {
        pending = config
        if (applying) return
        applying = true
        try {
            while (true) {
                // Stamped here rather than where the config was built, because this is the moment one really
                // goes out: a config superseded in the pending slot never gets a sequence, so the one that
                // does is the one an acknowledgement can name.
                val next = publication.stamping(pending ?: return)
                pending = null
                val acknowledgement = CompletableDeferred<ShizukuApplied>()
                this.acknowledgement = acknowledgement
                val applied = withTimeout(CONTROL_RESULT_DEADLINE) {
                    DaemonIpc.writeFrame(output, ShizukuSessionConfig.ADAPTER.encode(next))
                    // Raced against [ended], because [receive] can only fail the round trips it finds in
                    // flight: a slot installed after the reader is gone has nobody left to complete it, and
                    // awaiting it alone would spend the whole deadline rediscovering what [ended] already
                    // says. The deadline stays for what it was written for - a child still reading its
                    // socket that never answers - rather than standing in for a child that is not there.
                    // `select` is biased to its first clause, so an acknowledgement the reader delivered on
                    // its way out still wins the turn it shares with the stream ending.
                    select {
                        acknowledgement.onAwait { it }
                        ended.onAwait {
                            throw IOException("$BINARY_NAME stopped answering, so config " +
                                    "${next.sequence} will never be acknowledged")
                        }
                    }
                }
                check(applied.sequence == next.sequence) {
                    "$BINARY_NAME acknowledged config ${applied.sequence}, expected ${next.sequence}"
                }
                check(applied.downstream_epoch == next.downstream_epoch &&
                        applied.upstream_generation == next.upstream_generation) {
                    "$BINARY_NAME applied epoch ${applied.downstream_epoch}/generation " +
                            "${applied.upstream_generation}, sent ${next.downstream_epoch}/" +
                            next.upstream_generation
                }
                // Admission is part of the acknowledgement's contract, not a value read off it. The ordered
                // stop's first step is "stop admitting", and its whole point is that the app may then spend
                // up to a minute clearing the global preference before the child is fenced; a daemon that
                // acknowledged the right sequence and axes while still admitting would make that window a
                // lie. So a disagreement is a control failure like any other rather than a state update.
                check(applied.admitting == next.admit) {
                    "$BINARY_NAME is admitting ${applied.admitting} for config ${next.sequence}, " +
                            "which asked for ${next.admit}"
                }
            }
        } finally {
            applying = false
        }
    }

    /**
     * Awaits the acknowledgement of the config [apply] is currently waiting on. Null while none is in
     * flight, which is most of the time: a report can arrive whenever the daemon has something to say.
     */
    private var acknowledgement: CompletableDeferred<ShizukuApplied>? = null

    /**
     * Completed when [receive] stops, which is the first moment the app can tell the daemon is no longer
     * answering. Waiting for the next config to time out instead would leave a session believing it owned a
     * dataplane for a further [CONTROL_RESULT_DEADLINE].
     *
     * Read by [apply] as well as by the session's failure watcher, and for the same reason: this is the one
     * place that knows the conversation is over, so a control round trip consults it rather than waiting out
     * a deadline no answer can beat.
     */
    val ended = CompletableDeferred<Unit>()

    /**
     * Reads the daemon's side of the conversation for the session's whole life, rather than only while a
     * config is in flight. A background failure arrives when it happens, and the quiet stretches between
     * configs are exactly when one is most likely, so leaving it unread would both delay the report and
     * eventually stall the daemon's writer behind a full socket buffer.
     *
     * Ending is not a failure by itself - a stopped session closes this socket - but it is terminal for any
     * config still waiting, because nothing will ever acknowledge it now. Only the round trips in flight
     * when it ends are failed from here; one that begins afterwards finds no reader to fail it, which is why
     * [apply] races [ended] rather than trusting this to reach it.
     */
    private suspend fun receive() {
        try {
            while (true) {
                val frame = ShizukuDaemonFrame.ADAPTER.decode(DaemonIpc.readFrame(input))
                val report = frame.report
                if (report != null) {
                    // The same shape the root daemon's nonfatal reports take, because a structured failure
                    // means the same thing at either UID. Already coalesced daemon-side, so this cannot
                    // flood even though packet input is attacker-influenced.
                    val traced = DaemonException(report, daemonClassName = BINARY_NAME).withCurrentTrace()
                    Timber.tag(BINARY_NAME).w(traced)
                    SmartSnackbar.make(traced).show()
                    continue
                }
                val applied = frame.applied ?: throw IOException("$BINARY_NAME sent an empty frame")
                val acknowledgement = this.acknowledgement
                this.acknowledgement = null
                if (acknowledgement?.complete(applied) != true) {
                    throw IOException("$BINARY_NAME acknowledged config ${applied.sequence} unprompted")
                }
            }
        } catch (e: Exception) {
            if (e !is CancellationException) Timber.tag(BINARY_NAME).d(e)
            acknowledgement?.completeExceptionally(e)
            acknowledgement = null
            if (e is CancellationException) throw e
        } finally {
            ended.complete(Unit)
        }
    }

    companion object {
        private const val BINARY_NAME = "vpnhotspotd"

        private const val GRACEFUL_EXIT_SECONDS = 10L
        private const val SIGTERM_EXIT_SECONDS = 5L
        private const val SIGKILL_EXIT_SECONDS = 5L

        /**
         * One `write` for the whole frame, because every write while descriptors are set attaches them
         * again: a length prefix written byte by byte transfers the descriptor five times over.
         *
         * The socket is nonblocking, so a full send buffer would fail rather than wait. That cannot
         * happen for the first small frame on a fresh connection, so it is reported rather than retried
         * behind an invented delay: if it ever fires, the assumption was wrong and the failure says so.
         */
        private suspend fun writeFrameWithDescriptor(socket: LocalSocket, packet: ByteArray) {
            val frame = ByteBuffer.allocate(Int.SIZE_BYTES + packet.size).putInt(packet.size).put(packet)
            withContext(Dispatchers.IO) { socket.outputStream.write(frame.array()) }
        }

        /**
         * Starts the child and returns ownership of it, without yet talking to it. Split from [Child.connect]
         * so that the caller records the child in its own resource ledger *before* the handshake can fail:
         * `ProcessBuilder.start()` is the moment a process exists, not the moment it authenticates.
         */
        fun spawn(): Child {
            // same binary, same ABI check, same in-place APK exec as the root path
            val socketName = "$BINARY_NAME.${Process.myPid()}.${Random.nextLong().toHexString()}"
            val scope = CoroutineScope(privilegedDispatcher + SupervisorJob())
            val server = ALocalServerSocket(LocalServerSocket(socketName), Services.mainHandler)
            var process: ChildProcess? = null
            val pid = try {
                process = ProcessBuilder(DaemonController.daemonCommand + listOf("--app-uid", socketName))
                    .redirectErrorStream(true)
                    .start()
                // Read immediately, because everything below owns a process that can only be fenced by pid.
                // A launch whose pid cannot be read is rolled back here rather than handed on as a child
                // nothing can signal: SIGTERM through `destroy()` would still work, but the escalation that
                // exists precisely for a wedged child would not.
                process.pid.also { check(it > 0) { "$BINARY_NAME launched with pid $it" } }
            } catch (e: Throwable) {
                // Either nothing was launched, or it was and cannot be named; the second case is fenced here
                // and now, because no caller has been given anything to fence it with.
                process?.destroy()
                scope.cancel()
                try {
                    server.close()
                } catch (closing: IOException) {
                    e.addSuppressed(closing)
                }
                throw e
            }
            // drained without blocking cleanup, and a closed stream on exit is the normal end
            scope.launch(Dispatchers.IO) {
                try {
                    process.inputStream.bufferedReader().forEachLine { Timber.tag(BINARY_NAME).i(it) }
                } catch (e: IOException) {
                    Timber.tag(BINARY_NAME).d(e)
                }
            }
            return Child(process, pid, server, scope)
        }

        /**
         * Authenticates [child], hands it a duplicate of [tun] and waits for it to report ready.
         *
         * Transfers a duplicate and keeps the original open in the app process, which is what makes
         * app-process death close the daemon's copy too. The descriptor is set nonblocking before
         * duplicating, because the flag belongs to the shared file description; the daemon re-checks it
         * anyway, since this side cannot prove what arrived.
         *
         * Throwing leaves [child] exactly as it was: the caller's ledger still owns it and its fence still
         * has to run before the TUN is closed.
         */
        suspend fun connect(
            child: Child,
            tun: ParcelFileDescriptor,
            interfaceName: String,
            mtu: Int,
            publication: SessionPublication,
        ): AppUidDaemon {
            Os.fcntlInt(tun.fileDescriptor, OsConstants.F_SETFL,
                Os.fcntlInt(tun.fileDescriptor, OsConstants.F_GETFL, 0) or OsConstants.O_NONBLOCK)
            return withTimeout(CONTROL_RESULT_DEADLINE) {
                child.connect(tun, BootstrapConfig(interface_name = interfaceName, mtu = mtu), publication)
            }
        }
    }
}
