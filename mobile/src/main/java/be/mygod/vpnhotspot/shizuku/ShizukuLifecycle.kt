package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext

/**
 * The rootless mode's own command lane: one start or stop at a time, and never two sessions at once.
 *
 * Everything here is local to this mode. It does not consult, start, stop, delay or refuse root mode, and
 * root mode does not consult it: the two are independent, and when both run at once root's own per-interface
 * routing takes precedence over whatever upstream Android picked, without either side being told. What this
 * class serializes is only [Session]'s own resources - the TUN, the exact request, the agent, the global
 * preference and the child - which one session owns at a time and which a half-finished command must not
 * leave behind.
 *
 * Deliberately small, and deliberately without a framework in it: a session cannot be constructed without
 * Android, and this is the part that is only ordering.
 */
class ShizukuLifecycle(private val session: Session) {
    enum class State {
        /** No session, and no command running. */
        OFF,
        /**
         * A start has been accepted and its non-mutating preparation is running - Shizuku authorization above
         * all, which can sit on the user's own permission dialog for as long as they take.
         *
         * Nothing has been created yet, which is why this is cancellable and why an explicit stop arriving
         * during it simply supersedes the start rather than withdrawing a session.
         */
        PREPARING,
        /** Resources are being acquired and the session is being published. */
        PUBLISHING,
        /** The session is running. */
        ON,
        /**
         * The mode is going away and has not finished. Either a session is being withdrawn - reported as
         * gone only once every local resource is - or an explicit stop has superseded a preparation whose
         * leader has not observed that yet.
         *
         * The second case is a state rather than nothing because the leader still owns [starting]: reporting
         * off while a doomed flight is installed would let the next start join it and inherit its
         * [SupersededException].
         */
        RETIRING;

        /** True while a session exists or is being created. What this mode's own control renders as on. */
        val on get() = this != OFF

        /**
         * True while a command is in flight. Every surface renders this as busy rather than as either end
         * state, and offers no new command: it would not be lost, but offering it invites the user to fight
         * their own last press.
         */
        val busy get() = this == PREPARING || this == PUBLISHING || this == RETIRING
    }

    /** The one session this lane drives, supplied by its owner so neither re-enters this class. */
    interface Session {
        /**
         * Everything that has to hold before anything is created, and nothing that mutates anything: Shizuku
         * authorization, device support, and any cleanup a previous session still owed. Returns the
         * publication step.
         *
         * Split out because this is the slow, interactive half, and because a stop that arrives while it is
         * running has nothing to withdraw - which is what makes superseding it free.
         */
        suspend fun prepare(): suspend () -> Unit

        /**
         * Retires the session completely, including its child process, and everything the publication step
         * managed to create before it failed - which is why the publication step records what it creates
         * rather than unwinding it: this lane is the only rollback, so a failed one is retried by the next
         * explicit stop instead of immediately by the start that caused it.
         *
         * Idempotent, and throwing means something of it is still there - the child may still be relaying -
         * so the mode is *not* reported as off and the next stop is a retry rather than a fresh start.
         */
        suspend fun retire()
    }

    /** An explicit stop superseded a start that had created nothing yet. */
    class SupersededException : CancellationException("Superseded by an explicit stop")

    private val stateFlow = MutableStateFlow(State.OFF)
    val state = stateFlow.asStateFlow()

    /** Serializes the commands, so two of them cannot interleave their resources or their publications. */
    private val lane = Mutex()

    /**
     * The start in flight, so a duplicate press shares its outcome rather than beginning a second.
     *
     * The start runs on its *caller's* coroutine rather than on a scope of this class's own. That is what
     * keeps it cancellable in the ordinary way - the service that pressed it owns the job - and it is why
     * there is no internal scope to leak, join or dispatch onto.
     */
    private var starting: CompletableDeferred<Unit>? = null

    /**
     * Bumped by every explicit stop. A start samples it before its interactive half and refuses to publish if
     * it moved, which is how a stop supersedes a start that has created nothing yet - without either command
     * having to cancel the other's coroutine.
     */
    private var stops = 0L

    /**
     * Starts the mode, or shares a start already in flight.
     *
     * Cancellable through the interactive half only: once publication begins it finishes or rolls back,
     * because a half-published session is exactly the residue this exists to prevent.
     */
    suspend fun start() {
        var generation = 0L
        val done = CompletableDeferred<Unit>()
        // Awaited *outside* the lane: a follower that waited for the leader while holding it would be holding
        // the very lock the leader needs to publish with.
        lane.withLock {
            if (stateFlow.value == State.ON) return
            starting?.let { return@withLock it }
            generation = stops
            starting = done
            stateFlow.value = State.PREPARING
            null
        }?.let { return it.await() }
        var failure: Throwable? = null
        try {
            // Outside the lane on purpose: this waits on a human, and neither an unrelated follower nor the
            // opposite command may queue behind a permission dialog.
            val publish = session.prepare()
            // Everything from here on is one rollback boundary: cancelling would leave resources created
            // with nothing recording them.
            withContext(NonCancellable) {
                lane.withLock {
                    if (stops != generation) throw SupersededException()
                    stateFlow.value = State.PUBLISHING
                    try {
                        publish()
                    } catch (e: Throwable) {
                        // Rollback is this lane's, and only this lane's. A publication that threw may have
                        // left a child or an agent behind, and it records everything it created in the same
                        // ledger [Session.retire] withdraws - so retiring here is the one rollback, rather
                        // than a second one racing a rollback the publication ran for itself.
                        try {
                            session.retire()
                        } catch (retirement: Throwable) {
                            // Fail closed, for every kind of failure alike: [Session.retire] throwing means
                            // resources may remain, including when it throws a [CancellationException] - this
                            // is already inside [NonCancellable], so that is a deadline or a lost epoch
                            // rather than this caller going away. The mode keeps being reported as on, which
                            // is what makes the next explicit stop a retry rather than a fresh start, and
                            // both causes are reported.
                            e.addSuppressed(retirement)
                            stateFlow.value = State.ON
                            throw e
                        }
                        throw e
                    }
                    stateFlow.value = State.ON
                }
            }
        } catch (e: Throwable) {
            failure = e
        }
        // One critical section for both, because publishing the end state and retiring the flight are one
        // fact. Doing them apart left a window holding [State.OFF] with this - already doomed - deferred
        // still installed, in which a fresh start joined it and inherited its failure instead of running.
        withContext(NonCancellable) {
            lane.withLock {
                if (starting === done) {
                    starting = null
                    if (stateFlow.value != State.ON) stateFlow.value = State.OFF
                }
            }
        }
        failure?.let {
            done.completeExceptionally(it)
            throw it
        }
        done.complete(Unit)
    }

    /**
     * Withdraws the session, or returns at once if there is none.
     *
     * A stop that cannot confirm the session is gone leaves the state at [State.ON]: the child may still be
     * relaying, and reporting the mode as off then would be a lie.
     */
    suspend fun stop(): Unit = lane.withLock {
        // Recorded first so a start still in its interactive half supersedes itself when it gets there.
        stops++
        when (stateFlow.value) {
            State.OFF -> return@withLock
            // Nothing was ever created, so there is nothing to withdraw - but the superseded leader still
            // owns [starting], and it is the only thing that may publish [State.OFF]. Until it does, this is
            // a stop in progress, which is also what the user just asked for.
            //
            // [State.RETIRING] on entry can only be that same case: a real withdrawal holds this lane for
            // its whole duration, so no second command can observe one running. Repeating the stop is
            // therefore a no-op rather than a second supersession.
            State.PREPARING, State.RETIRING -> {
                stateFlow.value = State.RETIRING
                return@withLock
            }
            State.PUBLISHING, State.ON -> stateFlow.value = State.RETIRING
        }
        try {
            withContext(NonCancellable) { session.retire() }
        } catch (e: Throwable) {
            // Fail closed: the session may still be relaying, so it is not reported as gone.
            stateFlow.value = State.ON
            throw e
        }
        stateFlow.value = State.OFF
    }
}
