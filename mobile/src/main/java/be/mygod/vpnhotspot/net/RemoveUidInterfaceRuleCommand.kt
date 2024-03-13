package be.mygod.vpnhotspot.net

import android.content.Context
import android.os.Build
import android.os.IBinder
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.ParcelableBoolean
import be.mygod.librootkotlinx.RootCommand
import be.mygod.vpnhotspot.util.Services
import dalvik.system.PathClassLoader
import kotlinx.parcelize.Parcelize
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
            // only relevant for Android 11+ where com.android.tethering APEX is introduced
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

        private val serviceClassLoader by lazy {
            PathClassLoader(File("/apex/com.android.tethering/javalib/service-connectivity.jar").toURI().toURL()
                .toString(), "/apex/com.android.tethering/lib64${File.pathSeparator}/apex/com.android.tethering/lib",
                javaClass.classLoader)
        }
    }

    /**
     * Deprecated in Android 13: https://android.googlesource.com/platform/system/netd/+/android-13.0.0_r1/server/NetdNativeService.cpp#1142
     */
    object Impl29 {
        private val stub by lazy { findConnectivityClass("android.net.INetd\$Stub", serviceClassLoader) }
        private val netd by lazy {
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
    object JavaBpfMap {
        private val BpfMap by lazy { findConnectivityClass("com.android.net.module.util.BpfMap") }
        private val Struct by lazy { findConnectivityClass("com.android.net.module.util.Struct") }
        private val S32 by lazy { findConnectivityClass("com.android.net.module.util.Struct\$S32") }
        private val UidOwnerValue by lazy { findConnectivityClass("android.net.UidOwnerValue") }

        private val newS32 by lazy { S32.getDeclaredConstructor(Int::class.java) }
        private val newUidOwnerValue by lazy { UidOwnerValue.getDeclaredConstructor(Int::class.java, Long::class.java) }
        private val ruleUidOwnerValue by lazy { UidOwnerValue.getDeclaredField("rule") }
        private val iifMatch by lazy {
            findConnectivityClass("android.net.BpfNetMapsConstants").getDeclaredField("IIF_MATCH").getLong(null)
        }

        private val deleteEntry by lazy { BpfMap.getDeclaredMethod("deleteEntry", Struct) }
        private val updateEntry by lazy { BpfMap.getDeclaredMethod("updateEntry", Struct, Struct) }
        private val value by lazy { BpfMap.getDeclaredMethod("getValue", Struct) }

        private val uidOwnerMap by lazy {
            BpfMap.getDeclaredConstructor(String::class.java, Int::class.java, Class::class.java, Class::class.java)
                // BPF_F_RDWR
                .newInstance("/sys/fs/bpf/netd_shared/map_netd_uid_owner_map", 0, S32, UidOwnerValue)
        }
        operator fun invoke(uid: Int): Boolean {
            val uidS32 = newS32.newInstance(uid)
            val oldRule = ruleUidOwnerValue.getLong(value(uidOwnerMap, uidS32) ?: return false)
            return when {
                oldRule and iifMatch == 0L -> false
                oldRule == iifMatch -> deleteEntry(uidOwnerMap, uidS32) as Boolean
                else -> true.also {
                    updateEntry(uidOwnerMap, uidS32, newUidOwnerValue.newInstance(0, oldRule and iifMatch.inv()))
                }
            }
        }
    }

    @RequiresApi(33)
    object NativeBpfMap {
        private val BpfNetMaps by lazy { findConnectivityClass("com.android.server.BpfNetMaps", serviceClassLoader) }
        private val bpfNetMaps by lazy {
            try {
                // https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-14.0.0_r1/service/src/com/android/server/BpfNetMaps.java#335
                BpfNetMaps.getDeclaredConstructor(Context::class.java).newInstance(null)
            } catch (_: NoSuchMethodException) {
                // https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-13.0.0_r1/service/src/com/android/server/BpfNetMaps.java#57
                BpfNetMaps.getDeclaredConstructor().newInstance()
            }
        }

        /**
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-13.0.0_r1/service/src/com/android/server/BpfNetMaps.java#273
         */
        private val removeUidInterfaceRules by lazy {
            BpfNetMaps.getDeclaredMethod("native_removeUidInterfaceRules", IntArray::class.java)
                .apply { isAccessible = true }
        }
        operator fun invoke(uid: Int) {
            val ret = removeUidInterfaceRules(bpfNetMaps, intArrayOf(uid)) as Int
            check(ret == 0) { "native_removeUidInterfaceRules returns $ret" }
        }
    }

    override suspend fun execute() = ParcelableBoolean(if (Build.VERSION.SDK_INT < 33) {
        Impl29(uid)
        true
    } else try {
        JavaBpfMap(uid)
    } catch (e: Exception) {
        try {
            NativeBpfMap(uid)
            true
        } catch (e2: Exception) {
            e2.addSuppressed(e)
            throw e2
        }
    })
}
