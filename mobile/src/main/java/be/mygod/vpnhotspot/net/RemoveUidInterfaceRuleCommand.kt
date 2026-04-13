package be.mygod.vpnhotspot.net

import android.os.Build
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.ParcelableBoolean
import be.mygod.librootkotlinx.RootCommand
import be.mygod.vpnhotspot.root.Jni
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.lang.reflect.InvocationTargetException

/**
 * https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/server/NetdNativeService.cpp#1138
 */
@Parcelize
@RequiresApi(29)
data class RemoveUidInterfaceRuleCommand(private val uid: Int) : RootCommand<ParcelableBoolean> {
    /**
     * Netd is used before Android 13: https://android.googlesource.com/platform/system/netd/+/android-13.0.0_r1/server/NetdNativeService.cpp#1142
     */
    private object Impl29 {
        private val firewallRemoveUidInterfaceRules by lazy {
            Netd.getMethod("firewallRemoveUidInterfaceRules", IntArray::class.java)
        }
        operator fun invoke(uid: Int) {
            try {
                firewallRemoveUidInterfaceRules(Netd.service, intArrayOf(uid))
            } catch (e: InvocationTargetException) {
                if (e.cause?.message != "[Operation not supported on transport endpoint] : eBPF not supported") throw e
            }
        }
    }

    /**
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-13.0.0_r1/service/src/com/android/server/BpfNetMaps.java#209
     */
    @RequiresApi(33)
    private object JniBpfMap {
        private val constants by lazy { Netd.findConnectivityClass("android.net.BpfNetMapsConstants") }
        private val mapPath by lazy {
            try {
                constants.getDeclaredField("UID_OWNER_MAP_PATH").get(null) as String?
            } catch (e: ReflectiveOperationException) {
                if (Build.VERSION.SDK_INT >= 35) Timber.w(e)
                // https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-13.0.0_r1/bpf_progs/bpf_shared.h#146
                "/sys/fs/bpf/netd_shared/map_netd_uid_owner_map"
            }
        }
        private val matches by lazy {
            try {
                constants.getDeclaredField("IIF_MATCH").getLong(null) or
                        constants.getDeclaredField("LOCKDOWN_VPN_MATCH").getLong(null)
            } catch (e: ReflectiveOperationException) {
                if (Build.VERSION.SDK_INT >= 35) Timber.w(e)
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
