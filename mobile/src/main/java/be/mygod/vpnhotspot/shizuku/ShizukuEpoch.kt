package be.mygod.vpnhotspot.shizuku

import android.content.pm.PackageManager
import android.os.IBinder
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import rikka.shizuku.Shizuku
import rikka.shizuku.ShizukuBinderWrapper

/**
 * Every Shizuku-mode callback and privileged result is bounded by this, so a wedged remote cannot
 * strand a session start. Matches the tethering service's own listener timeout.
 */
internal const val CONTROL_RESULT_DEADLINE = 60_000L

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
        private const val PERMISSION_REQUEST_CODE = 0x5a75

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
            val binder = publication.binder ?: throw UnavailableException("Shizuku Binder unavailable")
            fun ensure(where: String) {
                if (!publisher.holds(publication)) {
                    throw UnavailableException("Shizuku $publication superseded $where")
                }
                if (Shizuku.getBinder() !== binder) throw UnavailableException("Shizuku Binder replaced")
                if (!binder.isBinderAlive) throw UnavailableException("Shizuku Binder died")
            }
            ensure("before the identity was read")
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
            ensure("while the identity was read")
            return ShizukuEpoch(publication, uid, opPackage)
        }

        suspend fun authorize(): ShizukuEpoch {
            listeners
            val publication = withTimeout(CONTROL_RESULT_DEADLINE) { publisher.awaitBinder() }
            return withContext(privilegedDispatcher) {
                if (Shizuku.isPreV11()) {
                    throw UnavailableException("Shizuku ${Shizuku.getVersion()} predates the permission API")
                }
                if (Shizuku.checkSelfPermission() != PackageManager.PERMISSION_GRANTED) {
                    val granted = CompletableDeferred<Int>()
                    val listener = Shizuku.OnRequestPermissionResultListener { requestCode, grantResult ->
                        if (requestCode == PERMISSION_REQUEST_CODE) granted.complete(grantResult)
                    }
                    Shizuku.addRequestPermissionResultListener(listener, Services.mainHandler)
                    try {
                        Shizuku.requestPermission(PERMISSION_REQUEST_CODE)
                        if (withTimeout(CONTROL_RESULT_DEADLINE) { granted.await() } !=
                            PackageManager.PERMISSION_GRANTED) {
                            throw UnavailableException("Shizuku permission denied")
                        }
                    } finally {
                        Shizuku.removeRequestPermissionResultListener(listener)
                    }
                }
                // The permission dialog above can sit on a human for a minute, and Shizuku may well have
                // been replaced or have died while it did, so nothing read before this point is trusted:
                // the identity is read as one bracketed unit against the publication being pinned.
                readIdentity(publication)
            }
        }
    }
}
