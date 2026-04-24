package be.mygod.vpnhotspot.net

import android.os.Build
import android.os.IBinder
import be.mygod.vpnhotspot.util.Services
import dalvik.system.PathClassLoader
import java.io.File

internal object Netd {
    private const val SERVICE_CONNECTIVITY_JAR = "/apex/com.android.tethering/javalib/service-connectivity.jar"

    private val classLoader by lazy {
        if (Build.VERSION.SDK_INT >= 30) {
            PathClassLoader(
                "$SERVICE_CONNECTIVITY_JAR${File.pathSeparator}/system/framework/services.jar",
                javaClass.classLoader,
            )
        } else PathClassLoader("/system/framework/services.jar", javaClass.classLoader)
    }

    val service by lazy {
        stubClass.getDeclaredMethod("asInterface", IBinder::class.java)(null, Services.netd)
    }

    fun getMethod(name: String, vararg parameterTypes: Class<*>) = interfaceClass.getMethod(name, *parameterTypes)

    fun findConnectivityClass(baseName: String, loader: ClassLoader? = javaClass.classLoader): Class<*> {
        // only relevant for Android 11+ where com.android.tethering APEX exists:
        // https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-12.0.0_r1/service/Android.bp#108
        if (Build.VERSION.SDK_INT >= 30) {
            try {
                // https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-13.0.0_r1/service/jarjar-rules.txt#5
                return Class.forName("android.net.connectivity.$baseName", true, loader)
            } catch (_: ClassNotFoundException) { }
            try {
                // https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-12.0.0_r1/service/jarjar-rules.txt#29
                return Class.forName("com.android.connectivity.$baseName", true, loader)
            } catch (_: ClassNotFoundException) { }
        }
        return Class.forName(baseName, true, loader)
    }

    private val stubClass by lazy { findConnectivityClass("android.net.INetd\$Stub", classLoader) }
    private val interfaceClass by lazy { findConnectivityClass("android.net.INetd", classLoader) }
}
