package be.mygod.vpnhotspot.root

import android.hardware.wifi.supplicant.ISupplicant
import android.os.Build
import android.os.IBinder
import android.os.IInterface
import dalvik.system.PathClassLoader
import timber.log.Timber

object SupplicantAidl {
    /**
     * Android 16's Wi-Fi mainline supplicant implementation introduced this binder service name; Android 17's
     * Wi-Fi P2P mainline supplicant implementation reuses it.
     *
     * Sources:
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-16.0.0_r1/service/java/com/android/server/wifi/mainline_supplicant/MainlineSupplicant.java#57
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-17.0.0_r1/service/java/com/android/server/wifi/p2p/SupplicantP2pIfaceHalAidlMainlineImpl.java#39
     */
    private const val MAINLINE_SUPPLICANT_SERVICE = "wifi_mainline_supplicant"
    /**
     * Android 16's Wi-Fi P2P AIDL implementation builds this as
     * `ISupplicant.DESCRIPTOR + "/default"` before calling `ServiceManager.waitForDeclaredService`.
     *
     * Sources:
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-16.0.0_r1/service/java/com/android/server/wifi/p2p/SupplicantP2pIfaceHalAidlImpl.java#106
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-16.0.0_r1/service/java/com/android/server/wifi/p2p/SupplicantP2pIfaceHalAidlImpl.java#358
     */
    private const val VENDOR_SUPPLICANT_SERVICE = "android.hardware.wifi.supplicant.ISupplicant/default"

    /**
     * Hidden service-manager API owner.
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-1.6_r1/core/java/android/os/ServiceManager.java#27
     */
    private val classServiceManager by lazy { Class.forName("android.os.ServiceManager") }
    /**
     * Hidden `ServiceManager.checkService(String)`: returns the binder if the named service already exists, or null,
     * without starting or waiting for a lazy service.
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-1.6_r1/core/java/android/os/ServiceManager.java#82
     */
    private val checkService by lazy { classServiceManager.getDeclaredMethod("checkService", String::class.java) }
    /**
     * Hidden system API `ServiceManager.waitForDeclaredService(String)`: returns null when the service is not declared;
     * otherwise servicemanager may start the service and wait for it to be ready.
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-11.0.0_r1/core/java/android/os/ServiceManager.java#256
     */
    private val waitForDeclaredService by lazy {
        classServiceManager.getDeclaredMethod("waitForDeclaredService", String::class.java)
    }
    /**
     * Android 16 defines `service-wifi` as an installable Java library in the `com.android.wifi` APEX's system-server
     * classpath fragment. That jar contains the mainline supplicant Java AIDL and jarjars generated vendor supplicant
     * classes to `com.android.wifi.x.*`.
     *
     * Sources:
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-16.0.0_r1/apex/Android.bp#60
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-16.0.0_r1/apex/Android.bp#143
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-16.0.0_r1/service/Android.bp#108
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-16.0.0_r1/service/Android.bp#138
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-16.0.0_r1/framework/jarjar-rules.txt#137
     */
    private val mainlineClassLoader by lazy {
        PathClassLoader("/apex/com.android.wifi/javalib/service-wifi.jar", javaClass.classLoader)
    }
    /**
     * Binder stub entry point used by Android 16's mainline supplicant implementation and Android 17's Wi-Fi P2P
     * mainline supplicant implementation.
     *
     * Sources:
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-16.0.0_r1/service/java/com/android/server/wifi/mainline_supplicant/MainlineSupplicant.java#80
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-17.0.0_r1/service/java/com/android/server/wifi/p2p/SupplicantP2pIfaceHalAidlMainlineImpl.java#67
     */
    private val asInterface by lazy {
        Class.forName("android.system.wifi.mainline_supplicant.IMainlineSupplicant\$Stub", true, mainlineClassLoader)
            .getDeclaredMethod("asInterface", IBinder::class.java)
    }
    /**
     * Android 17 AIDL method that exposes the stable vendor `ISupplicant` binder from the mainline supplicant service.
     * The Java return type inside `service-wifi.jar` is jarjar'd, so callers convert the returned binder back through
     * this app's generated stable `android.hardware.wifi.supplicant.ISupplicant` stub. Wi-Fi is APEX-delivered, so
     * [get] probes the live jar when `wifi_mainline_supplicant` is registered instead of using an SDK gate.
     *
     * Sources:
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-16.0.0_r1/aidl/mainline_supplicant/android/system/wifi/mainline_supplicant/IMainlineSupplicant.aidl#25
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-17.0.0_r1/service/java/com/android/server/wifi/p2p/SupplicantP2pIfaceHalAidlMainlineImpl.java#65
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-17.0.0_r1/aidl/mainline_supplicant/android/system/wifi/mainline_supplicant/IMainlineSupplicant.aidl#35
     * - https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/tags/android-17.0.0_r1/service/java/com/android/server/wifi/p2p/SupplicantP2pIfaceHalAidlMainlineImpl.java#139
     */
    private val getVendorSupplicant by lazy {
        Class.forName("android.system.wifi.mainline_supplicant.IMainlineSupplicant", true, mainlineClassLoader)
            .getDeclaredMethod("getVendorSupplicant")
    }

    val instance: ISupplicant? get() {
        if (Build.VERSION.SDK_INT < 30) return null
        try {
            return ISupplicant.Stub.asInterface((getVendorSupplicant(asInterface(null, checkService(null,
                MAINLINE_SUPPLICANT_SERVICE))) as IInterface).asBinder())
        } catch (e: Exception) {
            if (Build.VERSION.SDK_INT >= 37) Timber.w(e)
        }
        val binder = waitForDeclaredService(null, VENDOR_SUPPLICANT_SERVICE) as IBinder?
        return ISupplicant.Stub.asInterface(binder ?: return null)
    }
}
