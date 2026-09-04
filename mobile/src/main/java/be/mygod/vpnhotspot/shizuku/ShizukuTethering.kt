package be.mygod.vpnhotspot.shizuku

import android.net.IIntResultListener
import android.net.ITetheringConnector
import android.net.TetheringManager
import android.os.Build
import android.os.IBinder
import android.os.RemoteException
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import be.mygod.vpnhotspot.util.callSuper
import be.mygod.vpnhotspot.util.findIdentifier
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.selects.select
import kotlinx.coroutines.suspendCancellableCoroutine
import timber.log.Timber
import java.lang.reflect.InvocationHandler
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Method
import java.lang.reflect.Proxy
import kotlin.coroutines.resumeWithException

/** Direct connector owner for the global test-network preference and its process-death fence. */
internal class UnsupportedDeviceException(message: String, val expected: Boolean) : Exception(message)

@RequiresApi(30)
class PinnedTetheringConnector private constructor(
    private val epoch: ShizukuEpoch,
    private val binder: IBinder,
    private val connector: ITetheringConnector,
) {
    class DiedException(message: String) : Exception(message)

    private val recipient = IBinder.DeathRecipient { died.complete(Unit) }

    init {
        binder.linkToDeath(recipient, 0)
    }

    internal suspend fun setPreferTestNetworks(prefer: Boolean, debt: PreferenceResource) {
        val result = CompletableDeferred<Int>()
        val listener = object : IIntResultListener.Stub() {
            override fun onResult(resultCode: Int) {
                result.complete(resultCode).run { }
            }
        }
        epoch.ensureCurrent()
        if (prefer) debt.settingIssued() else debt.clearingIssued()
        try {
            try {
                UnblockCentral.ITetheringConnector_setPreferTestNetworks.invoke(connector, prefer, listener)
            } catch (e: InvocationTargetException) {
                throw e.targetException
            }
            val code = select<Int> {
                result.onAwait { it }
                died.onAwait { throw DiedException(app.getString(R.string.shizuku_failure_tethering_died)) }
            }
            if (code != TetheringManager.TETHER_ERROR_NO_ERROR) {
                if (prefer) debt.settingDenied() else debt.clearingDenied()
                throw TetheringManagerCompat.Failure(code)
            }
            epoch.ensureCurrent()
        } catch (e: Throwable) {
            if (!died.isCompleted) {
                if (e is TetheringManagerCompat.Failure) throw e
                if (prefer) debt.settingUnknown() else debt.clearingUnknown()
                throw e
            }
            debt.lostWithService()
            if (e is DiedException || e is CancellationException) throw e
            if (e !is Exception) throw e
            throw DiedException(app.getString(R.string.shizuku_failure_tethering_died)).apply {
                addSuppressed(e)
            }
        }
        if (prefer) debt.settingMutated() else debt.clearingConfirmed()
    }

    fun unlink() {
        if (!binder.unlinkToDeath(recipient, 0)) died.complete(Unit)
    }

    companion object {
        /**
         * TetheringManager permanently caches this connector, so process death is terminal until app restart.
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#467
         */
        val died = CompletableDeferred<Unit>()

        fun requireAutomaticUpstream() {
            if (Build.VERSION.SDK_INT >= 34) return
            val info = TetheringManagerCompat.resolvedService.serviceInfo
            val resources = app.packageManager.getResourcesForApplication(info.applicationInfo)
            val id = resources.findIdentifier("config_tether_upstream_automatic", "bool",
                "com.android.networkstack.tethering", info.packageName)
            if (id == 0) return Timber.w(Exception("config_tether_upstream_automatic not found"))
            if (!resources.getBoolean(id)) throw UnsupportedDeviceException(
                "This device does not use automatic tethering upstream selection", expected = true)
        }

        /**
         * Acquires through the hidden connector consumer; queued delivery catches `RemoteException` itself.
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-mainline-11.0.0_r45/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#361
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#365
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#480
         */
        suspend fun acquire(epoch: ShizukuEpoch) = suspendCancellableCoroutine { cont ->
            UnblockCentral.ITetheringConnector_asInterface
            UnblockCentral.ITetheringConnector_setPreferTestNetworks
            UnblockCentral.TetheringManager_ConnectorConsumer_onConnectorAvailable
            val handler = object : InvocationHandler {
                override fun invoke(proxy: Any, method: Method, args: Array<out Any?>?) = when {
                    method == UnblockCentral.TetheringManager_ConnectorConsumer_onConnectorAvailable -> {
                        val binder = (args!![0] as ITetheringConnector).asBinder()
                        try {
                            cont.resume(PinnedTetheringConnector(epoch, binder,
                                UnblockCentral.ITetheringConnector_asInterface(null, epoch.wrap(binder))
                                        as ITetheringConnector)) { _, value, _ -> value.unlink() }
                        } catch (e: RemoteException) {
                            died.complete(Unit)
                            cont.resumeWithException(DiedException(
                                app.getString(R.string.shizuku_failure_tethering_died)).apply {
                                    addSuppressed(e)
                                })
                        }
                    }
                    else -> callSuper(UnblockCentral.TetheringManager_ConnectorConsumer, proxy, method, args)
                }
            }
            UnblockCentral.TetheringManager_getConnector(Services.tethering, Proxy.newProxyInstance(
                UnblockCentral.TetheringManager_ConnectorConsumer.classLoader,
                arrayOf(UnblockCentral.TetheringManager_ConnectorConsumer), handler))
        }
    }
}
