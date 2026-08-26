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
import be.mygod.vpnhotspot.root.daemon.ApplyShizukuConfigCommand
import be.mygod.vpnhotspot.root.daemon.CancelCommand
import be.mygod.vpnhotspot.root.daemon.ClientEnvelope
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonEnvelope
import be.mygod.vpnhotspot.root.daemon.DaemonException
import be.mygod.vpnhotspot.root.daemon.DaemonIpc
import be.mygod.vpnhotspot.root.daemon.ShizukuApplied
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
 * The Shizuku-mode daemon, launched directly by the app process under the app UID. Not a Shizuku
 * UserService, not a root shell, not a persistent `app_process`, and not a JNI-hosted engine: the same
 * installed `vpnhotspotd` entry the root mode uses, exec'd in place from the APK.
 *
 * The privileged identity is deliberately absent here. Shell is barely better than the app UID for a
 * dataplane, since neither can touch netfilter, so nothing in this process needs Shizuku at all.
 *
 * # One conversation, not two
 *
 * What travels over the control socket is the same `ClientEnvelope`/`DaemonEnvelope` conversation the root
 * daemon speaks, with an app-UID command family of its own: the root vocabulary describes mutations that
 * need root, so sharing it would mean one message whose meaning depends on which UID reads it. What is
 * shared is the call lifecycle, and that is the point of sharing it - a Rust failure while receiving the
 * descriptor, validating it, or starting the dataplane arrives here as the structured [DaemonException] the
 * daemon built, rather than as a socket that closed and left the app to guess.
 *
 * The start call owns the session: [SESSION_CALL_ID] is written with the TUN attached, its event ACK is the
 * readiness this waits for, and every terminal frame for the session names it. Each config is an ordinary
 * one-shot call keyed to it.
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
     * Ownership begins at launch because the process exists before it connects. A failure after the start
     * call is written may also leave it holding a duplicate of the TUN. Once [spawn] returns, any later
     * failure leaves the caller's ledger to run the same exit fence as a normal stop before closing the TUN.
     */
    class Child internal constructor(
        private val process: ChildProcess,
        /**
         * Read from the launched process at `start()` and never from anything the peer says, because this is
         * both what the peer check compares against and what SIGKILL names. A child that never connects has
         * no peer credentials at all, and one that connects from another process is somebody else - so
         * taking the pid from the connection would make the check answer itself, let an impostor pass it,
         * and aim the fence at an innocent process.
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
         * What the child printed before it had a control channel to report over, which is the whole
         * diagnosis when it dies that early: a linker failure, a missing library, or a panic before `main`
         * reaches the socket leaves nothing structured behind.
         *
         * Kept only until the session starts, because from then on a failure is an `ErrorFrame` or a
         * nonfatal and this would just be an unbounded copy of the log. Read only after [draining] has been
         * joined, which is the happens-before edge that makes reading it from another lane sound.
         */
        private val output = StringBuilder()
        @Volatile
        private var capturing = true

        /** Drained without blocking cleanup, and a closed stream on exit is the normal end. */
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

        /**
         * Observed child exit, carrying what the exit says. Watched from launch rather than only while a
         * caller happens to be waiting, because the window this closes is the one *before* there is any
         * control channel at all: a child that dies then can answer nothing, and a start awaiting its
         * acknowledgement would otherwise wait on a process that has already gone.
         */
        internal val exited = scope.async {
            val status = process.awaitExit()
            // The child is gone, so its output stream is at EOF and this returns rather than waiting.
            draining.join()
            IOException(buildString {
                append(BINARY_NAME).append(' ').append(pid).append(" exited with ").append(status)
                if (output.isEmpty()) append(" without printing anything") else {
                    append(" after printing: ").append(output)
                }
            })
        }

        /**
         * Ordered shutdown, and a fence rather than a signal: everything downstream assumes the child is
         * gone, so this closes the control channel, gives the child its cleanup window, and does not return
         * until exit is observed.
         *
         * Closing the socket is the whole of the *request*. EOF on it is what the daemon's own config loop
         * reads as cancellation, and it is what makes that daemon cancel and join its dataplane, deliver
         * whatever it still owed and close its copy of the TUN before returning from `main`. No signal can
         * ask for that, and one sent early would replace it: the daemon installs no handler - its Tokio build
         * does not include the `signal` feature - so anything delivered to it terminates it outright. That is
         * why the escalation below runs only after the graceful window, never instead of it.
         *
         * The escalation itself is the process-cleanup policy [be.mygod.vpnhotspot.root.RootManager] already
         * applies to the root process librootkotlinx starts for it: [GRACEFUL_EXIT_SECONDS] for the ordered
         * teardown, then SIGTERM, then [SIGTERM_EXIT_SECONDS] more, then SIGKILL. The equivalence is of
         * policy and not of process - that one is a `su`/`app_process` handle reaped by a different owner,
         * and neither launches the other. These windows are a launched child's cleanup budget, not a
         * deadline on anything it answers - calls end on their result or on the owner's cancellation and
         * never on elapsed time - and the distinction matters here because this whole function is
         * [NonCancellable]: a child that will not act on EOF cannot be given up on by a user stop, and
         * waiting on it forever would leave it holding a duplicate of the TUN while this teardown, and every
         * successor that joins it, stayed fenced with nothing left that could recover it.
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
                    // Everything downstream treats the child as gone, so that has to be observed rather than
                    // assumed: an uninterruptible sleep survives SIGKILL, and reporting the session gone
                    // under a process still holding the TUN is exactly what this fence exists to prevent.
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
         * retained - above all not its pid, which stays the launched one, because a same-UID impostor whose
         * pid was adopted would become the peer whose socket is treated as the fence, while the real child
         * kept relaying with nothing left watching it.
         */
        internal suspend fun connect(
            tun: ParcelFileDescriptor,
            command: StartShizukuSessionCommand,
            publication: SessionPublication,
        ): AppUidDaemon {
            while (true) {
                // Raced against the child's exit *here and nowhere else*. Until a peer is authenticated there
                // is no conversation that could answer anything, so an exit is the only news there is; once
                // one is, that conversation is authoritative and a report it already enqueued must be read
                // before the socket's EOF is believed. Racing the whole start call against a pid and a
                // drained stdout would let two independent jobs decide the order, and the buffered frame
                // would lose.
                //
                // The accept runs on the child's own scope rather than on one this call owns, and is never
                // cancelled. [ALocalServerSocket.accept] hands its socket back out of a `withContext`, so a
                // cancellation delivered after the socket was accepted but before that value is dispatched
                // discards a descriptor nothing can reach afterwards - and a cancellation of this call
                // cancels a scope's children before this frame could shield them, so owning the job here
                // would not help. Ending it by closing the server instead has neither problem: the
                // descriptor's event awaiter cancels whatever is parked on it, an accept already inside the
                // syscall fails on the closed descriptor, and an accept already past both completes normally
                // and is closed below.
                val accepting = scope.async { server.accept() }
                val accepted = try {
                    select<ALocalSocket> {
                        accepting.onAwait { it }
                        exited.onAwait { throw it }
                    }
                } catch (e: Throwable) {
                    // The child exited, this call was cancelled, or the accept itself failed. The producer is
                    // still the only owner of whatever it may yet publish, so it is woken by closing this
                    // Child's own server and then drained to completion: a socket accepted a moment too late
                    // is closed here rather than left with nothing pointing at it. The fence closes this
                    // server again later, which is idempotent enough to log and carry on.
                    try {
                        server.close()
                    } catch (closing: IOException) {
                        e.addSuppressed(closing)
                    }
                    withContext(NonCancellable) {
                        val stranded = try {
                            accepting.await()
                        } catch (loser: Throwable) {
                            // Two endings are the close doing its job, and both are consumed: the event
                            // awaiter cancels a parked continuation, and an accept already in the syscall
                            // fails on the descriptor. `loser === e` is the accept having been what lost the
                            // race in the first place, where the exception being rethrown is this same
                            // instance and must not be suppressed into itself. Anything else is unexpected
                            // and is attached rather than allowed to replace the primary failure - a child
                            // exit is the useful diagnosis and stays the one thrown.
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
                    // Reads go through the nonblocking channel, because the accepted socket is
                    // nonblocking and a plain read returns EAGAIN. The one write does not, because
                    // ancillary descriptors are state of LocalSocketImpl's output stream: a channel that
                    // writes the raw descriptor by another route silently drops them.
                    val input = accepted.openReadChannel()
                    // Retained only now, with both checks passed: this is the socket whose EOF is the
                    // child's cancellation signal and the whole of the fence, so it must not be one an
                    // impostor holds.
                    socket = accepted
                    keep = true
                    // attaches to the next write on this socket, which is the start call below
                    tun.dup().use { duplicate ->
                        accepted.socket.setFileDescriptorsForSend(arrayOf(duplicate.fileDescriptor))
                        try {
                            writeFrameWithDescriptor(accepted.socket, ClientEnvelope.ADAPTER.encode(
                                ClientEnvelope(call_id = SESSION_CALL_ID, start_shizuku_session = command)))
                        } finally {
                            accepted.socket.setFileDescriptorsForSend(null)
                        }
                    }
                    val daemon = AppUidDaemon(input, accepted.openWriteChannel(), publication)
                    // Started only now, because everything above wrote rather than read. From here this is
                    // the only reader of that channel, and the start call's answer arrives through it.
                    scope.launch { daemon.receive() }
                    select<Unit> {
                        daemon.started.onAwait { }
                        // Raced for the same reason [apply] races it: a start the daemon can never answer -
                        // because it died, or because it answered with an error and then ended - has nothing
                        // left to complete, and awaiting the ACK alone would never learn that.
                        //
                        // The reader reaches EOF only after every frame ahead of it, so a start the daemon
                        // refused has already been parsed into [ended] by the time this fires. That is why
                        // the exit is consulted second and only here: it is the answer when the conversation
                        // ended without one, never a competitor to the answer it did give.
                        daemon.ended.onAwait { cause ->
                            throw cause ?: exited.await()
                        }
                    }
                    Timber.i("$BINARY_NAME ${credentials.pid} is ready on ${command.interface_name}")
                    // Authentication is complete and this child is the only accepted peer, and from here a
                    // failure is a frame rather than a line of startup output.
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
    private var pending: ShizukuSessionConfig? = null
    private var applying = false

    /**
     * Why nothing further may be written to this connection, once something may have gone out half-written.
     *
     * A transport fact about the socket, deliberately not a second copy of [ended]. [DaemonIpc.writeFrame]
     * writes the length, the payload and the flush at separate suspension points, so a write that failed or
     * was cancelled can leave a prefix with no frame boundary after it; appending anything to that asks the
     * daemon to parse garbage. What it records is therefore "this stream is unusable", which is a different
     * question from "how did the conversation end" - and keeping them apart is what lets the reader still
     * publish a structured error it had already buffered when the write failed.
     *
     * Never a cancellation, whatever ended the write: the ordered retirement observes this to decide whether
     * to fence the child, and it must not be handed a cancellation that was never its own. The caller that
     * hit the failure still gets its own exception unchanged.
     *
     * Confined to [privilegedDispatcher] with the rest of this class's call state, and set once: after the
     * first poison no further write is attempted, so there is no second failure to record.
     */
    private var poisoned: Throwable? = null

    /**
     * Sends one session config and waits for the daemon to reply with what it applied.
     *
     * Coalesced through a single pending slot rather than queued: the newest config is the whole truth, so
     * an older one has nothing left to say. The slot is necessary even though every caller runs on
     * [privilegedDispatcher], because this suspends while awaiting the reply and another observation can
     * reach the lane during that window - a single-threaded dispatcher orders dispatches, not
     * run-to-completion sections.
     *
     * A superseded caller returns normally instead of waiting for a reply that will never name its config.
     * The session was updated correctly, and parking that caller on an answer nobody will send would fence a
     * healthy dataplane behind it.
     */
    suspend fun apply(config: ShizukuSessionConfig) {
        pending = config
        if (applying) return
        applying = true
        try {
            while (true) {
                // Two different facts, checked before anything is stamped, allocated a call ID or written,
                // because either of them makes all three wrong: [SessionPublication] would burn a sequence
                // nobody can answer, and the frame would go onto a stream that must not carry one.
                //
                // [poisoned] first, because it is the one that makes writing itself impossible: a frame that
                // did not go out whole left a prefix behind, and nothing may follow it - not the next
                // observation's config, and not the ordered retirement's admission close, which has to fence
                // the child instead of appending to the wreckage.
                //
                // [ended] second, and only the reader completes it: it says the conversation is over and,
                // when the daemon explained itself, exactly why. Thrown as itself for the reason it is
                // carried at all - a refusal the daemon named says more than this side noticing the silence.
                poisoned?.let { throw it }
                if (ended.isCompleted) {
                    throw ended.await() ?: IOException("$BINARY_NAME is no longer answering")
                }
                // Stamped here rather than where the config was built, because this is the moment one really
                // goes out: a config superseded in the pending slot never gets a sequence, so the one that
                // does is the one a reply can name.
                val next = publication.stamping(pending ?: return)
                pending = null
                val id = nextCallId++
                val reply = CompletableDeferred<ShizukuApplied>()
                // The write and the wait are separate, because only one of them may be answered with a
                // cancel. [DaemonIpc.writeFrame] writes the length, the payload and the flush at separate
                // suspension points, so a failure or a cancellation inside it leaves a stream with no
                // boundary left to resynchronize on: appending a cancel would be writing into the middle of
                // a config, which repairs nothing and asks the daemon to parse garbage. Nothing was
                // registered either, so there is no call to abandon.
                try {
                    DaemonIpc.writeFrame(output, ClientEnvelope.ADAPTER.encode(ClientEnvelope(
                        call_id = id,
                        apply_shizuku_config = ApplyShizukuConfigCommand(SESSION_CALL_ID, next))))
                } catch (e: Exception) {
                    // The transport family only, exactly as wide as [abandon] catches: a raw
                    // `ErrnoException`, which is what this channel's own drain job cancels it with, and a
                    // `CancellationException` from a cancelled one both belong here. `Error` does not - a
                    // VM failure is not this conversation's to poison, to hold behind a reader, or to
                    // replace with a daemon report - so it propagates untouched.
                    //
                    // Recorded before it is rethrown, and that is what makes the prefix on the stream
                    // safe: the check at the top of this loop is what the ordered retirement's own
                    // `apply` hits next, so it fails instead of writing a config after half of one.
                    //
                    // Recorded on [poisoned] and deliberately *not* on [ended]. The daemon may already
                    // have enqueued a structured error the reader has not reached yet, and completing
                    // [ended] here would win that race permanently - first completion wins - leaving the
                    // session reported as a local write failure when the daemon had said exactly what
                    // went wrong. This side's write failing is a fact about the socket; why the
                    // conversation ended is the reader's to publish.
                    //
                    // Never recorded as a cancellation, whatever ended the write. The retirement that
                    // reads this has to see something it treats as a failure - so that it fences the
                    // child rather than rethrowing a cancellation that was never its own - and a
                    // cancelled write leaves exactly the same broken stream a failed one does.
                    if (poisoned == null) poisoned = if (e is CancellationException) {
                        IOException("$BINARY_NAME config ${next.sequence} was interrupted mid-frame", e)
                    } else e
                    // A cancellation is the caller's own and goes back promptly and unchanged. Anything
                    // else is the transport failing, and for *that* the reader is the authority rather
                    // than this side: the same broken socket that failed this write has already woken it,
                    // and the frame the daemon sent to explain itself may still be sitting unparsed in
                    // the receive buffer. Suspending here is what lets it run - the reader shares this
                    // lane, so finishing this call synchronously is precisely what used to beat it to
                    // [Session.failed] and put a local `EPIPE` in front of the user instead of the
                    // daemon's own report.
                    //
                    // The wait ends when the reader does, which a broken socket makes immediate: it is
                    // already readable, so it drains what is buffered and then reaches EOF. Nothing bounds
                    // it - no sleep, no poll, no grace period - because the reader ending is the event
                    // being waited for and nothing may stand in for it: a reader that somehow never
                    // finished leaves this call fenced rather than reporting a failure of this line's own
                    // invention. A reader with nothing to add answers null and this call still fails as
                    // itself.
                    if (e !is CancellationException) ended.await()?.let { throw it }
                    throw e
                }
                // Registered only now, with the whole frame out. Safe without a lock because the reader
                // shares this lane: the write returns and this runs before either can suspend again, and
                // no answer to this call can exist before the frame carrying it did.
                acknowledgement = Acknowledgement(id, reply)
                val applied = try {
                    // Raced against [ended], because [receive] can only fail the round trips it finds in
                    // flight: a slot installed after the reader is gone has nobody left to complete it,
                    // and awaiting it alone would park on a reply nobody is left to send while [ended]
                    // already says why. A child that is still reading its socket and simply never answers
                    // is the other case, and the only thing that ends this wait then is the user's stop.
                    // `select` is biased to its first clause, so a reply the reader delivered on its way
                    // out still wins the turn it shares with the stream ending.
                    select<ShizukuApplied> {
                        reply.onAwait { it }
                        // The cause is thrown as itself when there is one, and that is the whole point of
                        // carrying it: a session the daemon ended with a structured error is one whose
                        // report names the context, the errno and the Rust line, and `readableMessage`
                        // reaches through a [DaemonException] to it because that is a `RemoteException`.
                        // Wrapping it in an `IOException` would stop that unwrapping dead and put this
                        // sentence in front of the user instead of the daemon's own. The sentence is
                        // therefore only for the ending that explains nothing - the stream stopping with
                        // no attributable frame - where naming the config that will never be answered is
                        // the most that can be said.
                        ended.onAwait { cause ->
                            throw cause ?: IOException("$BINARY_NAME stopped answering, so config " +
                                    "${next.sequence} will never be acknowledged")
                        }
                    }
                } catch (e: CancellationException) {
                    // This session's own withdrawal cancelling the observer that called this, which is the
                    // only thing that ends this wait short of an answer. The command is out and registered,
                    // so this is exactly the case root's controller cancels a call in: nobody is waiting on
                    // it any more, and the daemon is told to stop counting on it. The config itself still
                    // applied - this conversation is serial, so the daemon reads that cancel strictly after
                    // it answered - which is why nothing rewinds the axes here, and why the reader drops
                    // the reply that arrives.
                    withContext(NonCancellable) { abandon(id) }
                    throw e
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
                // Admission is part of the reply's contract, not a value read off it. The ordered stop's
                // first step is "stop admitting", and its whole point is that the app may then spend as long
                // as the tethering service takes clearing the global preference before the child is fenced;
                // a daemon that acknowledged the right sequence and axes while still admitting would make
                // that window a lie. So a disagreement is a control failure like any other rather than a
                // state update.
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
     * Drops the pending slot for an abandoned config call and tells the daemon so, which is what root's
     * controller does for a cancelled caller. Best effort on the write: a control socket that can no longer
     * carry it is already the end of this session, and [ended] is what reports that.
     *
     * Only ever reached with the config frame fully written, so this is a whole frame on a stream that has a
     * boundary: a cancellation *inside* that write never gets here, because there is nothing left to
     * resynchronize on and this could only make it worse.
     */
    private suspend fun abandon(id: Long) {
        if (acknowledgement?.id == id) acknowledgement = null
        try {
            DaemonIpc.writeFrame(output, ClientEnvelope.ADAPTER.encode(
                ClientEnvelope(call_id = id, cancel = CancelCommand())))
        } catch (e: Exception) {
            // Deliberately the whole transport family rather than [IOException]. This channel's drain job
            // stores the raw `ErrnoException` it failed on and cancels the channel with it, so that is what a
            // later write rethrows - and a channel cancelled any other way surfaces a `CancellationException`
            // instead. Either escaping here would be worse than the write failing: this runs under
            // [NonCancellable] inside the caller's own catch, so it would replace that caller's own
            // cancellation, and it would leave the stream unpoisoned although the cancel frame may be half
            // written. `Error` is deliberately not caught; a VM failure is not this conversation's to absorb.
            //
            // The same boundary a config frame has, and normalized the same way: a cancellation is recorded
            // as a transport cause so the ordered retirement fences rather than rethrowing it. Not on
            // [ended] - the reader owns what ended the conversation - and this returns either way, so the
            // caller's original exception is what propagates.
            if (poisoned == null) poisoned = if (e is CancellationException) {
                IOException("$BINARY_NAME call $id could not be cancelled", e)
            } else e
            Timber.tag(BINARY_NAME).d(e)
        }
    }

    /** The next config call's ID. Never [SESSION_CALL_ID], which the start call keeps for the session. */
    private var nextCallId = SESSION_CALL_ID + 1

    /** One config call in flight, which is all this ever has: [apply] coalesces the rest away. */
    private class Acknowledgement(val id: Long, val reply: CompletableDeferred<ShizukuApplied>)

    /**
     * The config call [apply] is currently waiting on. Null while none is in flight, which is most of the
     * time: a report or a terminal frame can arrive whenever the daemon has something to say.
     */
    private var acknowledgement: Acknowledgement? = null

    /** Completed by the start call's event ACK, which is the daemon's readiness. */
    private val started = CompletableDeferred<Unit>()

    /**
     * How the conversation ended, and why when there is a why: the attributable failure the daemon named or
     * the reader refused, or null for an ending that explains nothing - control-socket EOF, a cancelled
     * reader, a clean completion.
     *
     * Reader-owned. [receive] is the only thing that completes it, and that exclusivity is half of what
     * makes the daemon's own explanation authoritative: completion is first-wins, so a writer completing it
     * on a local failure would permanently outrank a structured error the daemon had already enqueued but
     * the reader had not yet parsed. A write that cannot go out is recorded on [poisoned] instead, which
     * answers a different question.
     *
     * The other half is that a writer whose transport failed *waits* for this before reporting - see
     * [apply]. Reader-only completion alone would not have been enough, because the failing writer reaches
     * the session's own terminal first and the two share one dispatcher; suspending on this is what hands
     * the lane to the reader that is already awake on the same broken socket.
     *
     * Completing it is the only moment the app can tell the daemon is no longer answering. Read by [apply] -
     * both before it writes and while it waits - by [Child.connect], and by the session's failure watcher,
     * all for that reason: this is the one place that knows the conversation is over, so a control round trip
     * consults it rather than parking forever on an answer that can no longer come.
     */
    val ended = CompletableDeferred<Throwable?>()

    /**
     * Reads the daemon's side of the conversation for the session's whole life, rather than only while a
     * config is in flight. A background failure arrives when it happens, and the quiet stretches between
     * configs are exactly when one is most likely, so leaving it unread would both delay the report and
     * eventually stall the daemon's writer behind a full socket buffer.
     *
     * Ending is not a failure by itself - a stopped session closes this socket - but it is terminal for any
     * config still waiting, because nothing will ever answer it now. Only the round trips in flight when it
     * ends are failed from here; one that begins afterwards finds no reader to fail it, which is why [apply]
     * races [ended] rather than trusting this to reach it.
     */
    private suspend fun receive() {
        var cause: Throwable? = null
        try {
            while (true) {
                // Split from everything below it, and that split is the classification. The stream simply
                // ending is the conversation being over: it says nothing beyond that it happened, the child's
                // own exit status and output are the diagnosis there, and [Child.connect] is what reaches for
                // them. Everything else - a length prefix that names no frame, a payload that will not
                // decode, a frame this side refuses - is something the daemon did, and is carried out of here
                // as itself.
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
                        // The same shape the root daemon's nonfatal reports take, because a structured
                        // failure means the same thing at either UID. Already coalesced daemon-side, so this
                        // cannot flood even though packet input is attacker-influenced.
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
                        // Terminal whichever call it names, and that is the daemon's contract rather than a
                        // precaution: the session call's error *is* the session ending, and a config the
                        // daemon refused ends the session too - it answers that call and stops. So this is
                        // the session's cause either way. Reading on instead would leave the EOF a moment
                        // later to complete [ended] with nothing, and the generic message that produces
                        // would then race the report that says why.
                        cause = exception
                        // Answered under its own call first, so a waiter gets the error named for the call
                        // it made; the cleanup below would otherwise hand it the same exception anyway, and
                        // an error naming a call this side already abandoned is dropped there with a
                        // warning rather than delivered to whoever is waiting now.
                        if (id != SESSION_CALL_ID) takeAcknowledgement(id)?.completeExceptionally(exception)
                        return
                    }
                    envelope.reply != null -> {
                        val frame = envelope.reply
                        val id = frame.call_id.readCallId()
                        // Read before the slot is taken, so a reply this side cannot use still leaves the
                        // call in flight for the cleanup below to fail rather than stranding it.
                        val applied = frame.shizuku_applied
                            ?: throw IOException("$BINARY_NAME answered a config call with $frame")
                        takeAcknowledgement(id)?.complete(applied)
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
                        // Completing a call that was never acknowledged is the daemon claiming a session it
                        // never started, which is the same protocol error root reports as a call that
                        // "completed before event" - and unlike the case below it leaves [Child.connect]
                        // with nothing to return.
                        if (!started.isCompleted) {
                            throw IOException("$BINARY_NAME completed the session before starting it")
                        }
                        // A clean end of the session call: the dataplane finished on its own while the app
                        // was still connected. Not a failure, so nothing is carried out of here.
                        return
                    }
                    else -> throw IOException("$BINARY_NAME sent an empty frame")
                }
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            // A frame this side could not act on, which says something the app can use and which the child -
            // still alive, still holding the TUN - will not say again. Carried rather than logged away: the
            // call in flight needs it, and so does a startup still waiting on the ACK, which would otherwise
            // wait indefinitely on a process that is not going to exit.
            Timber.tag(BINARY_NAME).w(e)
            cause = e
        } finally {
            // Whatever ended this - a terminal frame, EOF, a protocol failure or cancellation - the call in
            // flight can no longer be answered.
            acknowledgement?.reply?.completeExceptionally(cause
                ?: IOException("$BINARY_NAME stopped answering"))
            acknowledgement = null
            // Null only for the endings that explain nothing: control-socket EOF, a cancelled reader, and a
            // clean completion. Everything else - the daemon's own structured error, and a frame this side
            // refused - is what the session reports instead of a generic message, and what stops a startup
            // from waiting on an exit that is not coming.
            ended.complete(cause)
        }
    }

    /**
     * Rejects a call ID no call can have, on every frame that carries one.
     *
     * Zero is what an unset proto field decodes to and negative is what a `uint64` past `Long.MAX_VALUE`
     * reads back as; both are the same refusal root's controller makes. Dropping such a frame as merely
     * unmatched would leave the call it was meant for waiting on an answer that has already been sent and
     * discarded, which is exactly the failure mode this conversation exists to remove.
     */
    private fun Long.readCallId(): Long {
        if (this <= 0) throw IOException("Invalid $BINARY_NAME call id $this")
        return this
    }

    /**
     * The config call [id] answers, or null when it names one nobody is waiting on: a call this side
     * abandoned is answered all the same, because the daemon reads the cancel only after it has replied.
     */
    private fun takeAcknowledgement(id: Long): CompletableDeferred<ShizukuApplied>? {
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

        /**
         * The start call, and the one ID neither side has to be told: it is written before there is anything
         * to allocate from, and every other call on this connection is keyed to it.
         */
        private const val SESSION_CALL_ID = 1L

        /**
         * The launched child's cleanup budget, matching the windows [be.mygod.vpnhotspot.root.RootManager]
         * gives the root process librootkotlinx starts for it - the same policy applied to a different
         * process, not a claim that either is the other. The resource being bounded is a process this app
         * started and still owns, not a call it is waiting on an answer to. The graceful window is what the
         * ordered teardown gets - cancel and join the dataplane, deliver what is owed, close the TUN copy -
         * and the SIGTERM window only covers the default disposition of a signal the daemon has no handler
         * for, so reaching either means the child is not acting on what it was told.
         *
         * [SIGKILL_EXIT_SECONDS] bounds an assertion rather than a wait for cooperation: nothing but an
         * uninterruptible sleep outlives SIGKILL, and that has to be reported rather than waited out,
         * because everything after this fence treats the child as gone.
         */
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
         * so that the caller records the child in its own resource ledger *before* the start call can fail:
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
                // Read immediately, because the pid is what authenticates the connection this child will
                // make: the accepted socket's peer credentials are checked against it, and a launch whose pid
                // cannot be read is rolled back here rather than handed on as a child no accept could ever
                // tell apart from another process of this same app.
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
            return Child(process, pid, server, scope)
        }

        /**
         * Authenticates [child], hands it a duplicate of [tun] on the session's start call and waits for the
         * daemon to acknowledge that call.
         *
         * Transfers a duplicate and keeps the original open in the app process, which is what makes
         * app-process death close the daemon's copy too. The descriptor is set nonblocking before
         * duplicating, because the flag belongs to the shared file description; the daemon re-checks it
         * anyway, since this side cannot prove what arrived.
         *
         * The wait is raced against the child's own exit, because until a peer connects there is no call to
         * answer and no report to read: nothing else will ever connect, so a child that died in the linker or
         * before `main` would otherwise never be discovered at all.
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
            return child.connect(tun,
                StartShizukuSessionCommand(interface_name = interfaceName, mtu = mtu), publication)
        }
    }
}
