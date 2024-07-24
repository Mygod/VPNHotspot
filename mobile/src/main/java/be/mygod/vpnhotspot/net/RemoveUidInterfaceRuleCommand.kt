package be.mygod.vpnhotspot.net

import android.os.Build
import android.os.IBinder
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.ParcelableBoolean
import be.mygod.librootkotlinx.RootCommand
import be.mygod.vpnhotspot.root.Jni
import be.mygod.vpnhotspot.util.Services
import dalvik.system.PathClassLoader
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.io.File

/**
 * https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/server/NetdNativeService.cpp#1138
 */
@Parcelize
@RequiresApi(29)
data class RemoveUidInterfaceRuleCommand(private val uid: Int) : RootCommand<ParcelableBoolean> {
    @Suppress("JAVA_CLASS_ON_COMPANION")
    companion object {
        private fun findConnectivityClass(baseName: String, loader: ClassLoader? = javaClass.classLoader): Class<*> {
            // only relevant for Android 11+ where com.android.tethering APEX exists
            if (Build.VERSION.SDK_INT >= 30) {
                try {
                    // https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-14.0.0_r1/service/Android.bp#333
                    return Class.forName("android.net.connectivity.$baseName", true, loader)
                } catch (_: ClassNotFoundException) { }
                try {
                    // https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-12.0.0_r1/service/jarjar-rules.txt#29
                    return Class.forName("com.android.connectivity.$baseName", true, loader)
                } catch (_: ClassNotFoundException) { }
            }
            return Class.forName(baseName, true, loader)
        }
    }

    /**
     * Netd is used before Android 13: https://android.googlesource.com/platform/system/netd/+/android-13.0.0_r1/server/NetdNativeService.cpp#1142
     */
    private object Impl29 {
        private val stub by lazy {
            findConnectivityClass("android.net.INetd\$Stub", if (Build.VERSION.SDK_INT >= 30) {
                PathClassLoader(
                    "/apex/com.android.tethering/javalib/service-connectivity.jar${File.pathSeparator}/system/framework/services.jar",
//                    "/apex/com.android.tethering/lib64${File.pathSeparator}/apex/com.android.tethering/lib",
                    javaClass.classLoader)
            } else PathClassLoader("/system/framework/services.jar", javaClass.classLoader))
        }
        val netd by lazy {
            stub.getDeclaredMethod("asInterface", IBinder::class.java)(null, Services.netd)
        }
        private val firewallRemoveUidInterfaceRules by lazy {
            stub.getMethod("firewallRemoveUidInterfaceRules", IntArray::class.java)
        }
        operator fun invoke(uid: Int) = firewallRemoveUidInterfaceRules(netd, intArrayOf(uid))
    }

    /**
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-14.0.0_r1/service/src/com/android/server/BpfNetMaps.java#416
     */
    @RequiresApi(33)
    private object JniBpfMap {
        private val constants by lazy { findConnectivityClass("android.net.BpfNetMapsConstants") }
        private val mapPath by lazy {
            try {
                constants.getDeclaredField("UID_OWNER_MAP_PATH").get(null) as String?
            } catch (e: ReflectiveOperationException) {
                Timber.w(e)
                // https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-13.0.0_r1/bpf_progs/bpf_shared.h#146
                "/sys/fs/bpf/netd_shared/map_netd_uid_owner_map"
            }
        }
        private val matches by lazy {
            try {
                constants.getDeclaredField("IIF_MATCH").getLong(null) or
                        constants.getDeclaredField("LOCKDOWN_VPN_MATCH").getLong(null)
            } catch (e: ReflectiveOperationException) {
                Timber.w(e)
                // https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-13.0.0_r1/bpf_progs/bpf_shared.h#160
                3 shl 7
            }
        }

        operator fun invoke(uid: Int) = Jni.removeUidInterfaceRules(mapPath, uid, matches)
    }

    override suspend fun execute() = ParcelableBoolean(if (Build.VERSION.SDK_INT < 33) {
        Impl29(uid)
        true
    } else JniBpfMap(uid))
}
