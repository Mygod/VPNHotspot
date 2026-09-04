package be.mygod.vpnhotspot.shizuku

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.job
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Owns one cancellable lifespan. Successors join the process-wide predecessor, while each lifespan's
 * non-cancellable finalizer alone withdraws intent, retires its resources, and reports whether they fenced.
 */
class ShizukuLifecycle(private val scope: CoroutineScope, private val session: Session) {
    interface Session {
        suspend fun settle()

        suspend fun prepare(): suspend () -> Unit

        suspend fun awaitEnd()

        suspend fun retire(owner: Job)

        fun publish(owner: Job)

        fun withdraw(owner: Job)

        fun report(e: Exception)

        suspend fun fenced(): Boolean

        fun settled(fenced: Boolean)
    }

    companion object {
        private var installed: Job? = null
    }

    private var lifespan: Job? = null

    private enum class Owed {
        NOTHING,

        INHERITED,

        OWN,
    }

    private fun install(live: Boolean) {
        val predecessor = installed
        val job = scope.launch(start = CoroutineStart.LAZY) {
            val self = coroutineContext.job
            var owed = if (live) Owed.NOTHING else Owed.INHERITED
            try {
                if (live) try {
                    predecessor?.join()
                    session.settle()
                    val publish = session.prepare()
                    owed = Owed.OWN
                    publish()
                    session.awaitEnd()
                } catch (_: CancellationException) {
                } catch (e: Exception) {
                    session.report(e)
                }
            } finally {
                withContext(NonCancellable) {
                    try {
                        session.withdraw(self)
                        predecessor?.join()
                        try {
                            when (owed) {
                                Owed.NOTHING -> {}
                                Owed.INHERITED -> session.settle()
                                Owed.OWN -> session.retire(self)
                            }
                        } catch (e: CancellationException) {
                            session.report(e)
                        } catch (e: Exception) {
                            session.report(e)
                        }
                    } finally {
                        var fenced = false
                        try {
                            try {
                                fenced = session.fenced()
                            } catch (e: CancellationException) {
                                session.report(e)
                            } catch (e: Exception) {
                                session.report(e)
                            }
                        } finally {
                            if (installed === self) installed = null
                            if (lifespan === self) {
                                lifespan = null
                                session.settled(fenced)
                            }
                        }
                    }
                }
            }
        }
        installed = job
        lifespan = job
        if (live) session.publish(job)
        job.start()
    }

    fun start() = install(true)

    fun housekeep() = install(false)

    fun stop() = stop(lifespan)

    private fun stop(job: Job?) {
        if (job == null) return
        session.withdraw(job)
        job.cancel()
    }

    fun destroy() {
        val outgoing = lifespan
        install(false)
        stop(outgoing)
    }

    val idle get() = lifespan == null
}
