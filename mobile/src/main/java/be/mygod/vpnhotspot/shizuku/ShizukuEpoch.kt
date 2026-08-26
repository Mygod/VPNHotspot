package be.mygod.vpnhotspot.shizuku

import android.content.pm.PackageManager
import android.os.IBinder
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.withContext
import moe.shizuku.server.IShizukuService
import rikka.shizuku.Shizuku
import rikka.shizuku.ShizukuBinderWrapper

/**
 * Wrapped transactions are blocking Binder calls that must stay ordered behind one owner, so all
 * privileged work runs on this single lane and never on Main.
 */
internal val privilegedDispatcher = Dispatchers.Default.limitedParallelism(1, "shizuku-privileged")

/**
 * One pinned Shizuku Binder identity.
 *
 * [ShizukuBinderWrapper] dispatches through mutable process-local Shizuku state, so a replacement or
 * death between two transactions would silently retarget them. The pinned identity is therefore
 * compared before and after every privileged operation; a mismatch at either boundary makes that
 * operation's result unknown, and an unknown result is never a commit.
 */
class ShizukuEpoch private constructor(
    /**
     * The whole publication this identity was pinned from, kept as one object rather than as a binder and a
     * generation: the two are only meaningful together, and a check that took one from here and the other
     * from a later read would report "unchanged" across exactly the replacement it exists to catch.
     */
    private val publication: BinderPublisher.Publication<IBinder>,
    val uid: Int,
    /**
     * ConnectivityService verifies the operation package against the Binder calling UID through
     * `AppOpsManager.checkPackage`, and `AppOpsService` rejects a package that does not belong to
     * that uid, while uid 0 skips the package check entirely. A shell-backed session must therefore
     * present shell's package; a root-backed one may keep the app's.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#9330
     * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-17.0.0_r1/services/core/java/com/android/server/appop/AppOpsService.java#5218
     * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-17.0.0_r1/services/core/java/com/android/server/appop/AppOpsService.java#5119
     */
    val opPackageName: String,
) {
    class UnavailableException(message: String) : Exception(message)

    /**
     * The pinned identity no longer matches, so whatever the bracketed operation did or returned is
     * unknown. Callers must roll back everything that operation could have created.
     */
    class ChangedException(message: String) : Exception(message)

    private val binder get() = checkNotNull(publication.binder) { "$publication carries no binder" }

    fun ensureCurrent() {
        if (!publisher.holds(publication)) {
            throw ChangedException("Shizuku $publication superseded by ${publisher.current}")
        }
        // Shizuku can swap its binder before its own listener has run, which no publication would have
        // noticed yet; this catches that window without ever mixing the two sources, because it compares the
        // binder this epoch was pinned to rather than adopting whatever is there now.
        if (Shizuku.getBinder() !== binder) throw ChangedException("Shizuku Binder replaced")
        if (!binder.isBinderAlive) throw ChangedException("Shizuku Binder died")
    }

    /**
     * Bracket a complete privileged operation rather than only its individual transactions, because
     * framework code issues transactions the owner cannot check one by one.
     *
     * [dispose] releases whatever the call returned when the closing check fails: the operation may
     * well have succeeded against the superseded epoch, so its result is live but unusable, and the
     * caller has not yet recorded it for rollback.
     */
    fun <T> bracket(dispose: (T) -> Unit = { }, block: () -> T): T {
        ensureCurrent()
        val result = block()
        try {
            ensureCurrent()
        } catch (e: ChangedException) {
            try {
                dispose(result)
            } catch (disposal: Throwable) {
                e.addSuppressed(disposal)
            }
            throw e
        }
        return result
    }

    /**
     * Forward a system service's transactions through this epoch. The wrapper reports null from
     * `queryLocalInterface`, so `Stub.asInterface` builds a proxy that transacts through Shizuku
     * instead of resolving to a local interface, and it links death on the wrapped service rather
     * than on Shizuku itself.
     */
    fun wrap(service: IBinder): IBinder = ShizukuBinderWrapper(service)

    companion object {
        /** Owns uid 2000 and is the only package a shell identity can attribute operations to. */
        private const val SHELL_PACKAGE = "com.android.shell"

        /**
         * Authorization checks the identity's permissions rather than the app's: every privileged
         * call in this mode authorizes against the Binder calling UID, which is Shizuku's.
         * `NETWORK_SETTINGS` is checked here even though only the tethering preference needs it, so
         * an identity that cannot complete the mode is rejected before anything is created. AOSP
         * shell holds all three, but they are OEM-variable grants.
         */
        private val requiredPermissions = arrayOf(
            "android.permission.MANAGE_TEST_NETWORKS",
            "android.permission.CONNECTIVITY_USE_RESTRICTED_NETWORKS",
            "android.permission.NETWORK_SETTINGS",
        )

        private val publisher = BinderPublisher<IBinder>()

        /**
         * Ordinary in-process provider delivery: Shizuku multiprocess support stays off and
         * `requestBinderForNonProviderProcess` is never called. Both listeners are sticky in effect
         * and live for the process, and they only publish one snapshot each so they cannot root a
         * session.
         */
        private val listeners by lazy {
            Shizuku.addBinderReceivedListenerSticky(Shizuku.OnBinderReceivedListener {
                Shizuku.getBinder()?.let { publisher.received(it) } ?: publisher.died()
            }, Services.mainHandler)
            Shizuku.addBinderDeadListener(Shizuku.OnBinderDeadListener {
                publisher.died()
            }, Services.mainHandler)
        }

        /**
         * The three questions that decide whether a publication may still be acted on at all, asked
         * wherever this authorization is about to read something or commit a side effect against it. Shared
         * rather than repeated, because "known stale" has to mean the same thing at every one of those
         * points: the publication being pinned is still the current one, Shizuku's own binder is still that
         * publication's, and that binder is alive. Answers with the pinned binder, which is the only one
         * anything here may transact through.
         */
        private fun ensurePinned(publication: BinderPublisher.Publication<IBinder>, where: String): IBinder {
            val binder = publication.binder ?: throw UnavailableException("Shizuku Binder unavailable")
            if (!publisher.holds(publication)) {
                throw UnavailableException("Shizuku $publication superseded $where")
            }
            if (Shizuku.getBinder() !== binder) throw UnavailableException("Shizuku Binder replaced")
            if (!binder.isBinderAlive) throw UnavailableException("Shizuku Binder died")
            return binder
        }

        /**
         * Everything an identity is decided by, read against one publication and rejected if that
         * publication is superseded at any point along the way.
         *
         * The reads are separate calls into Shizuku's process-local state, and each of them is answered by
         * whatever binder is current *then* rather than by the one this authorization is pinning. So a
         * replacement or a redelivery landing between two of them would produce an epoch whose uid, package
         * and permissions describe one identity while its binder is another - which is exactly the
         * combination every later `ensureCurrent` is supposed to catch and could no longer, because the
         * mismatch would already be baked in. Bracketing them is what makes the answer one identity's.
         */
        private fun readIdentity(publication: BinderPublisher.Publication<IBinder>): ShizukuEpoch {
            // This app's own authorization, asked of the pinned binder itself. `Shizuku.checkSelfPermission`
            // would answer from a process-global flag that an *earlier* service's grant can have left set,
            // and otherwise transact through Shizuku's mutable current service - so it can report an
            // authorization that belongs to an identity this epoch is not being pinned to. Read here rather
            // than only where the request is issued, because being authorized is part of what an identity is
            // and the grant is what the dialog above was waiting for.
            if (!IShizukuService.Stub.asInterface(ensurePinned(publication, "before the identity was read"))
                    .checkSelfPermission()) {
                throw UnavailableException("Shizuku $publication has not authorized this app")
            }
            val uid = Shizuku.getUid()
            for (permission in requiredPermissions) {
                if (Shizuku.checkRemotePermission(permission) != PackageManager.PERMISSION_GRANTED) {
                    throw UnavailableException("Shizuku identity uid $uid lacks $permission")
                }
            }
            val opPackage = if (uid == 0) app.packageName else {
                val packages = app.packageManager.getPackagesForUid(uid)
                packages?.find { it == SHELL_PACKAGE } ?: packages?.firstOrNull()
                    ?: throw UnavailableException("No package owns Shizuku identity uid $uid")
            }
            ensurePinned(publication, "while the identity was read")
            return ShizukuEpoch(publication, uid, opPackage)
        }

        suspend fun authorize(): ShizukuEpoch {
            listeners
            val publication = publisher.awaitBinder()
            return withContext(privilegedDispatcher) {
                if (Shizuku.isPreV11()) {
                    throw UnavailableException("Shizuku ${Shizuku.getVersion()} predates the permission API")
                }
                // Both of these go to the binder this authorization pinned, never to whatever
                // `Shizuku.checkSelfPermission` and `Shizuku.requestPermission` would pick out of Shizuku's
                // mutable current service: a replacement landing between the two would otherwise launch the
                // dialog against a successor nothing here has validated, and answer the question about it.
                if (!IShizukuService.Stub.asInterface(
                        ensurePinned(publication, "before its permission was checked")).checkSelfPermission()) {
                    val attempt = publisher.Attempt(publication)
                    val listener = Shizuku.OnRequestPermissionResultListener(attempt::deliver)
                    Shizuku.addRequestPermissionResultListener(listener, Services.mainHandler)
                    try {
                        // Refused here rather than only once the answer is in, because the request is a side
                        // effect Shizuku offers no way to retract: a publication already known stale must
                        // launch no dialog at all, and neither must a lifespan the user has already stopped.
                        // Registering the listener above suspends nowhere, so a stop landing in that
                        // interval has no other point at which to be noticed before the dialog is up.
                        //
                        // Asked twice deliberately. The first is for precedence: a stop and a stale
                        // publication can both be true, and a caller that pressed stop should get its own
                        // cancellation rather than the superseded-Shizuku failure that would be reported to
                        // them instead. The second is the last statement before the transaction, because
                        // validating the publication and building the proxy are themselves work a stop can
                        // land in the middle of. What is left between that check and the transaction cannot
                        // be closed from here at all - Shizuku takes no cancellation - so a stop or a
                        // replacement inside it can still leave a dialog whose answer this authorization
                        // will refuse.
                        ensureActive()
                        val service = IShizukuService.Stub.asInterface(
                            ensurePinned(publication, "before its permission request was issued"))
                        ensureActive()
                        service.requestPermission(attempt.token)
                        // Waited on for as long as the human takes: the dialog is theirs to answer whenever
                        // they like, and exactly three things end this wait - their answer, the exact
                        // publication that was asked being superseded, and the lifespan being cancelled by a
                        // stop. The middle one is not a bound on the human but a terminal fact about *this
                        // authorization*: what has gone is the identity the request belonged to, so even a
                        // grant could no longer produce an epoch here.
                        //
                        // It is not that nothing could still deliver the answer. Shizuku hands every service
                        // the same process-global application binder and dispatches every result to one
                        // process-global listener list, so a replaced-but-live service can still answer a
                        // request this attempt has given up on - which is what the attempt refuses, by its
                        // own token and by taking nothing at all once its publication has gone. A successor
                        // is never quietly authorized against: re-asking is a new authorization rather than
                        // this one continuing.
                        val result = attempt.await() ?: throw UnavailableException(
                            "Shizuku $publication superseded by ${publisher.current} with its permission " +
                                    "request outstanding")
                        if (result != PackageManager.PERMISSION_GRANTED) {
                            throw UnavailableException("Shizuku permission denied")
                        }
                    } finally {
                        Shizuku.removeRequestPermissionResultListener(listener)
                    }
                }
                // The permission dialog above can sit on a human indefinitely, and Shizuku may well have
                // been replaced or have died while it did, so nothing read before this point is trusted:
                // the identity is read as one bracketed unit against the publication being pinned.
                readIdentity(publication)
            }
        }
    }
}
