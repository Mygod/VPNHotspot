package be.mygod.vpnhotspot.shizuku

import android.content.Context
import android.content.ContextWrapper
import android.net.ConnectivityManager
import android.net.IConnectivityManager
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import java.lang.reflect.Modifier

/**
 * Constructor-free privileged [ConnectivityManager] clone used only for the exact request, its release, and
 * agent registration. All fields except `mContext` and `mService` alias the ordinary process manager.
 * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-11.0.0_r1/core/java/android/net/ConnectivityManager.java#2394
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/ConnectivityManager.java#2626
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#2944
 */
@RequiresApi(30)
class PrivilegedConnectivity private constructor(
    val manager: ConnectivityManager,
    val service: IConnectivityManager,
    val context: Context,
) {
    private class PrivilegedContext(base: Context, private val opPackage: String) : ContextWrapper(base) {
        lateinit var manager: ConnectivityManager

        override fun getSystemService(name: String): Any? =
            if (name == Context.CONNECTIVITY_SERVICE) manager else super.getSystemService(name)
        override fun getOpPackageName() = opPackage
        override fun getAttributionTag(): String? = null
    }

    companion object {
        /**
         * Reflects `Unsafe.theUnsafe` and `allocateInstance(Class)`; the unsupported shape is stable on 11-17.
         * https://android.googlesource.com/platform/libcore/+/refs/tags/android-11.0.0_r1/ojluni/src/main/java/sun/misc/Unsafe.java#55
         * https://android.googlesource.com/platform/libcore/+/refs/tags/android-13.0.0_r1/ojluni/src/main/java/sun/misc/Unsafe.java#57
         * https://android.googlesource.com/platform/libcore/+/refs/tags/android-17.0.0_r1/ojluni/src/main/java/sun/misc/Unsafe.java#63
         */
        private val classUnsafe by lazy { Class.forName("sun.misc.Unsafe") }
        private val theUnsafe by lazy {
            classUnsafe.getDeclaredField("theUnsafe").apply { isAccessible = true }.get(null)
        }
        private val allocateInstance by lazy {
            classUnsafe.getDeclaredMethod("allocateInstance", Class::class.java)
        }

        fun create(epoch: ShizukuEpoch): PrivilegedConnectivity {
            val fieldContext = UnblockCentral.ConnectivityManager_mContext
            val fieldService = UnblockCentral.ConnectivityManager_mService
            val fieldInstance = UnblockCentral.ConnectivityManager_sInstance
            val ordinary = Services.connectivity
            val ordinaryService = fieldService.get(ordinary) as IConnectivityManager
            checkNotNull(fieldContext.get(ordinary)) { "Ordinary ConnectivityManager has no context" }
            UnblockCentral.ConnectivityManager_isFeatureEnabled?.invoke(ordinary, 0L)
            val singleton = fieldInstance.get(null)
            val context = PrivilegedContext(app, epoch.opPackageName)
            val service = IConnectivityManager.Stub.asInterface(epoch.wrap(ordinaryService.asBinder()))
            val manager = allocateInstance(theUnsafe, ConnectivityManager::class.java) as ConnectivityManager
            for (field in ConnectivityManager::class.java.declaredFields) {
                if (Modifier.isStatic(field.modifiers)) continue
                field.isAccessible = true
                field.set(manager, field.get(ordinary))
            }
            fieldContext.set(manager, context)
            fieldService.set(manager, service)
            check(fieldContext.get(manager) === context) { "Privileged manager rejected its context" }
            check(fieldService.get(manager) === service) { "Privileged manager rejected its service" }
            check(fieldInstance.get(null) === singleton) { "ConnectivityManager singleton was written" }
            check(app.getSystemService(ConnectivityManager::class.java) === ordinary) {
                "Ordinary context stopped returning the ordinary manager"
            }
            context.manager = manager
            return PrivilegedConnectivity(manager, service, context)
        }
    }
}
