package be.mygod.vpnhotspot.net

import android.os.IBinder
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.util.Services
import dalvik.system.PathClassLoader
import java.io.File

@RequiresApi(31)
object Netd {
    private val classLoader by lazy {
        PathClassLoader("/apex/com.android.tethering/javalib/service-connectivity.jar${
            File.pathSeparator}/system/framework/services.jar", javaClass.classLoader)
    }

    val service by lazy {
        findConnectivityClass("android.net.INetd\$Stub", classLoader).getDeclaredMethod("asInterface",
            IBinder::class.java)(null, Services.netd)
    }

    /**
     * https://android.googlesource.com/platform/system/netd/+/android-12.0.0_r1/server/binder/android/net/INetd.aidl#397
     * https://android.googlesource.com/platform/frameworks/base/+/android-12.0.0_r1/services/core/java/com/android/server/IpSecService.java#1883
     */
    val ipSecUpdateSecurityPolicy by lazy {
        findConnectivityClass("android.net.INetd", classLoader).getMethod(
            "ipSecUpdateSecurityPolicy",
            Int::class.javaPrimitiveType,
            Int::class.javaPrimitiveType,
            Int::class.javaPrimitiveType,
            String::class.java,
            String::class.java,
            Int::class.javaPrimitiveType,
            Int::class.javaPrimitiveType,
            Int::class.javaPrimitiveType,
            Int::class.javaPrimitiveType,
        )
    }

    private fun findConnectivityClass(baseName: String, loader: ClassLoader): Class<*> {
        // only relevant for Android 11+ where com.android.tethering APEX exists:
        // https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-12.0.0_r1/service/Android.bp#108
        try {
            // https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-13.0.0_r1/service/jarjar-rules.txt#5
            return Class.forName("android.net.connectivity.$baseName", true, loader)
        } catch (_: ClassNotFoundException) { }
        try {
            // https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-12.0.0_r1/service/jarjar-rules.txt#29
            return Class.forName("com.android.connectivity.$baseName", true, loader)
        } catch (_: ClassNotFoundException) { }
        return Class.forName(baseName, true, loader)
    }
}
