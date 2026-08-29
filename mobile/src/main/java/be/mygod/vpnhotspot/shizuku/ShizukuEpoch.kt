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

internal val privilegedDispatcher = Dispatchers.Default.limitedParallelism(1, "shizuku-privileged")

/**
 * Pins privileged work to one Shizuku binder and effective UID. [bracket] validates both sides of a
 * synchronous mutation and disposes its result if the publication changed meanwhile.
 */
class ShizukuEpoch private constructor(
    private val publication: BinderPublisher.Publication<IBinder>,
    val uid: Int,
    val opPackageName: String,
) {
    class UnavailableException(message: String) : Exception(message)

    class ChangedException(message: String) : Exception(message)

    private val binder get() = checkNotNull(publication.binder) { "$publication carries no binder" }

    fun ensureCurrent() {
        if (!publisher.holds(publication)) {
            throw ChangedException("Shizuku $publication superseded by ${publisher.current}")
        }
        if (Shizuku.getBinder() !== binder) throw ChangedException("Shizuku Binder replaced")
        if (!binder.isBinderAlive) throw ChangedException("Shizuku Binder died")
    }

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

    fun wrap(service: IBinder): IBinder = ShizukuBinderWrapper(service)

    companion object {
        private const val SHELL_PACKAGE = "com.android.shell"

        private val requiredPermissions = arrayOf(
            "android.permission.MANAGE_TEST_NETWORKS",
            "android.permission.CONNECTIVITY_USE_RESTRICTED_NETWORKS",
            "android.permission.NETWORK_SETTINGS",
        )

        private val publisher = BinderPublisher<IBinder>()

        private val listeners by lazy {
            Shizuku.addBinderReceivedListenerSticky(Shizuku.OnBinderReceivedListener {
                Shizuku.getBinder()?.let { publisher.received(it) } ?: publisher.died()
            }, Services.mainHandler)
            Shizuku.addBinderDeadListener(Shizuku.OnBinderDeadListener {
                publisher.died()
            }, Services.mainHandler)
        }

        private fun ensurePinned(publication: BinderPublisher.Publication<IBinder>, where: String): IBinder {
            val binder = publication.binder ?: throw UnavailableException("Shizuku Binder unavailable")
            if (!publisher.holds(publication)) {
                throw UnavailableException("Shizuku $publication superseded $where")
            }
            if (Shizuku.getBinder() !== binder) throw UnavailableException("Shizuku Binder replaced")
            if (!binder.isBinderAlive) throw UnavailableException("Shizuku Binder died")
            return binder
        }

        private fun readIdentity(publication: BinderPublisher.Publication<IBinder>): ShizukuEpoch {
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
                if (!IShizukuService.Stub.asInterface(
                        ensurePinned(publication, "before its permission was checked")).checkSelfPermission()) {
                    val attempt = publisher.Attempt(publication)
                    val listener = Shizuku.OnRequestPermissionResultListener(attempt::deliver)
                    Shizuku.addRequestPermissionResultListener(listener, Services.mainHandler)
                    try {
                        ensureActive()
                        val service = IShizukuService.Stub.asInterface(
                            ensurePinned(publication, "before its permission request was issued"))
                        ensureActive()
                        service.requestPermission(attempt.token)
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
                readIdentity(publication)
            }
        }
    }
}
