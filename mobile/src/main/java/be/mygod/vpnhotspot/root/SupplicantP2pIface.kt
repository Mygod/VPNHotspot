package be.mygod.vpnhotspot.root

import android.hardware.wifi.supplicant.IfaceType
import android.os.IHwBinder
import android.os.IHwInterface
import android.os.RemoteException
import be.mygod.librootkotlinx.ParcelableThrowable
import be.mygod.vpnhotspot.util.callSuper
import be.mygod.vpnhotspot.util.matches
import dalvik.system.PathClassLoader
import java.io.File
import java.lang.reflect.InvocationHandler
import java.lang.reflect.Method
import java.lang.reflect.Proxy

object SupplicantP2pIface {
    class Hidl12UnsupportedException : UnsupportedOperationException("P2P supplicant HIDL 1.2 missing")

    private val classLoader by lazy {
        PathClassLoader(listOf(
            // https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/android-10.0.0_r1/service/Android.mk#46
            // https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/android-10.0.0_r1/service/Android.mk#58
            "/system/framework/wifi-service.jar",
            // https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/android-11.0.0_r1/service/Android.bp#101
            // https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/android-11.0.0_r1/service/Android.bp#104
            "/system/framework/service-wifi.jar",
            // https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/heads/android16-qpr2-release/apex/Android.bp#61
            // https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/heads/android16-qpr2-release/apex/Android.bp#146
            // https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/heads/android16-qpr2-release/apex/Android.bp#149
            "/apex/com.android.wifi/javalib/service-wifi.jar",
            // https://android.googlesource.com/platform/frameworks/base/+/android-10.0.0_r1/data/etc/platform.xml#222
            "/system/framework/android.hidl.base-V1.0-java.jar",
        ).filter { File(it).isFile }.joinToString(File.pathSeparator), javaClass.classLoader)
    }

    private val supplicantPackage by lazy {
        var missingClass: Throwable? = null
        for (packagePrefix in listOf(
            "com.android.wifi.x.android.hardware.wifi.supplicant",
            "android.hardware.wifi.supplicant",
        )) try {
            return@lazy packagePrefix to Class.forName("$packagePrefix.V1_0.ISupplicant", true, classLoader)
        } catch (e: ClassNotFoundException) {
            missingClass = e
        }
        throw missingClass!!
    }
    private val packagePrefix get() = supplicantPackage.first
    private val iSupplicant get() = supplicantPackage.second

    private val listInterfacesCallback by lazy {
        Class.forName("$packagePrefix.V1_0.ISupplicant\$listInterfacesCallback", true, classLoader)
    }
    private val getInterfaceCallback by lazy {
        Class.forName("$packagePrefix.V1_0.ISupplicant\$getInterfaceCallback", true, classLoader)
    }
    private val iSupplicantIface by lazy { Class.forName("$packagePrefix.V1_0.ISupplicantIface", true, classLoader) }
    private val classIfaceInfo by lazy {
        Class.forName("$packagePrefix.V1_0.ISupplicant\$IfaceInfo", true, classLoader)
    }
    private val iSupplicantP2pIface by lazy {
        Class.forName("$packagePrefix.V1_2.ISupplicantP2pIface", true, classLoader)
    }
    private val classSupplicantStatus by lazy {
        Class.forName("$packagePrefix.V1_0.SupplicantStatus", true, classLoader)
    }
    private val listInterfaces by lazy { iSupplicant.getDeclaredMethod("listInterfaces", listInterfacesCallback) }
    private val getInterface by lazy {
        iSupplicant.getDeclaredMethod("getInterface", classIfaceInfo, getInterfaceCallback)
    }
    private val asInterface by lazy {
        Class.forName("$packagePrefix.V1_0.ISupplicantP2pIface", true, classLoader).getDeclaredMethod(
            "asInterface", IHwBinder::class.java)
    }
    private val castFrom by lazy { iSupplicantP2pIface.getDeclaredMethod("castFrom", IHwInterface::class.java) }
    private val addGroup by lazy {
        iSupplicantP2pIface.getDeclaredMethod("addGroup_1_2", ArrayList::class.java, String::class.java,
            Boolean::class.javaPrimitiveType, Int::class.javaPrimitiveType, ByteArray::class.java,
            Boolean::class.javaPrimitiveType)
    }
    private val setMacRandomization by lazy {
        iSupplicantP2pIface.getDeclaredMethod("setMacRandomization", Boolean::class.javaPrimitiveType)
    }
    private val getService by lazy { iSupplicant.getDeclaredMethod("getService") }
    private val name by lazy { classIfaceInfo.getDeclaredField("name") }
    private val type by lazy { classIfaceInfo.getDeclaredField("type") }
    private val code by lazy { classSupplicantStatus.getDeclaredField("code") }
    private val debugMessage by lazy { classSupplicantStatus.getDeclaredField("debugMessage") }
    private fun requireSuccess(status: Any?, operation: String) = code.getInt(status).let { code ->
        if (code != 0) throw RemoteException("P2P supplicant HIDL $operation failed: $code (${debugMessage[status]})")
    }

    /**
     * Android 10's named P2P group path casts to HIDL 1.2 and calls `addGroup_1_2`;
     * older HIDL 1.0/1.1 cannot carry SSID/passphrase/frequency arguments.
     *
     * Android 10 packages the generated supplicant HIDL classes into `wifi-service.jar` under
     * their original package names. Android 11+ Wi-Fi module builds jarjar the same generated
     * classes into `com.android.wifi.x.*` inside `service-wifi.jar`.
     *
     * https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/android-10.0.0_r1/service/Android.mk#46
     * https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/android-11.0.0_r1/service/Android.bp#79
     * https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/heads/android16-qpr2-release/framework/jarjar-rules.txt#141
     * https://android.googlesource.com/platform/packages/modules/Wifi/+/refs/heads/android16-qpr2-release/service/java/com/android/server/wifi/p2p/SupplicantP2pIfaceHalHidlImpl.java#1406
     * https://android.googlesource.com/platform/hardware/interfaces/+/android-10.0.0_r1/wifi/supplicant/1.2/ISupplicantP2pIface.hal#68
     */
    fun addGroup(ssid: ByteArray, passphrase: String, frequency: Int, randomizeMac: Boolean): ParcelableThrowable? {
        val supplicant = getService(null)
        var ifaces: ArrayList<*>? = null
        listInterfaces(supplicant, Proxy.newProxyInstance(classLoader, arrayOf(listInterfacesCallback),
            object : InvocationHandler {
                override fun invoke(proxy: Any, method: Method, args: Array<out Any?>?) = when {
                    method.matches("onValues", classSupplicantStatus, ArrayList::class.java) -> {
                        requireSuccess(args!![0], "listInterfaces")
                        ifaces = args[1] as ArrayList<*>?
                        null
                    }
                    else -> callSuper(listInterfacesCallback, proxy, method, args)
                }
            }))
        val p2pInfo = ifaces!!.first { it != null && type.getInt(it) == IfaceType.P2P }
        val p2pName = name[p2pInfo]
        var iface: IHwInterface? = null
        getInterface(supplicant, p2pInfo, Proxy.newProxyInstance(classLoader, arrayOf(getInterfaceCallback),
            object : InvocationHandler {
                override fun invoke(proxy: Any, method: Method, args: Array<out Any?>?) = when {
                    method.matches("onValues", classSupplicantStatus, iSupplicantIface) -> {
                        requireSuccess(args!![0], "getInterface($p2pName)")
                        iface = args[1] as IHwInterface
                        null
                    }
                    else -> callSuper(getInterfaceCallback, proxy, method, args)
                }
            }))
        val p2pIface = castFrom(null, asInterface(null, iface!!.asBinder()))
            ?: throw Hidl12UnsupportedException()
        val macRandomizationError = try {   // best-effort: keep group creation working even if this transaction is unavailable
            requireSuccess(setMacRandomization(p2pIface, randomizeMac), "setMacRandomization")
            null
        } catch (e: Exception) {
            e
        }
        try {
            requireSuccess(addGroup(p2pIface,
                ArrayList<Byte>(ssid.size).apply { for (b in ssid) add(b) },
                passphrase, true, frequency, ByteArray(6), false), "addGroup_1_2")
            return macRandomizationError?.let { ParcelableThrowable(it) }
        } catch (e: Throwable) {
            macRandomizationError?.let { e.addSuppressed(it) }
            throw e
        }
    }
}
