package be.mygod.vpnhotspot.util

import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.content.*
import android.content.res.Resources
import android.net.InetAddresses
import android.net.LinkProperties
import android.net.RouteInfo
import android.os.Build
import android.os.RemoteException
import android.system.Os
import android.text.Spannable
import android.text.SpannableString
import android.text.SpannableStringBuilder
import android.view.MenuItem
import android.view.View
import android.widget.ImageView
import androidx.annotation.DrawableRes
import androidx.annotation.RequiresApi
import androidx.core.net.toUri
import androidx.core.view.isVisible
import androidx.databinding.BindingAdapter
import androidx.fragment.app.DialogFragment
import androidx.fragment.app.FragmentManager
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.io.File
import java.io.FileNotFoundException
import java.lang.invoke.MethodHandles
import java.lang.reflect.InvocationHandler
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Method
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketException

tailrec fun Throwable.getRootCause(): Throwable {
    if (this is InvocationTargetException || this is RemoteException) return (cause ?: return this).getRootCause()
    return this
}
val Throwable.readableMessage: String get() = getRootCause().run { localizedMessage ?: javaClass.name }

/**
 * This is a hack: we wrap longs around in 1 billion and such. Hopefully every language counts in base 10 and this works
 * marvelously for everybody.
 */
fun Long.toPluralInt(): Int {
    check(this >= 0)    // please don't mess with me
    if (this <= Int.MAX_VALUE) return toInt()
    return (this % 1000000000).toInt() + 1000000000
}

fun Context.ensureReceiverUnregistered(receiver: BroadcastReceiver) {
    try {
        unregisterReceiver(receiver)
    } catch (_: IllegalArgumentException) { }
}

fun DialogFragment.showAllowingStateLoss(manager: FragmentManager, tag: String? = null) {
    if (!manager.isStateSaved) show(manager, tag)
}

fun broadcastReceiver(receiver: (Context, Intent) -> Unit) = object : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) = receiver(context, intent)
}

fun intentFilter(vararg actions: String) = IntentFilter().also { actions.forEach(it::addAction) }

@BindingAdapter("android:src")
fun setImageResource(imageView: ImageView, @DrawableRes resource: Int) = imageView.setImageResource(resource)

@BindingAdapter("android:visibility")
fun setVisibility(view: View, value: Boolean) {
    view.isVisible = value
}

fun makeIpSpan(ip: InetAddress) = ip.hostAddress.let {
    // exclude all bogon IP addresses supported by Android APIs
    if (!app.hasTouch || ip.isMulticastAddress || ip.isAnyLocalAddress || ip.isLoopbackAddress ||
            ip.isLinkLocalAddress || ip.isSiteLocalAddress || ip.isMCGlobal || ip.isMCNodeLocal ||
            ip.isMCLinkLocal || ip.isMCSiteLocal || ip.isMCOrgLocal) it else SpannableString(it).apply {
        setSpan(CustomTabsUrlSpan("https://ipinfo.io/$it"), 0, length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
    }
}
fun makeMacSpan(mac: String) = if (app.hasTouch) SpannableString(mac).apply {
    setSpan(CustomTabsUrlSpan("https://macvendors.co/results/$mac"), 0, length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
} else mac

fun NetworkInterface.formatAddresses(macOnly: Boolean = false) = SpannableStringBuilder().apply {
    try {
        val address = hardwareAddress?.let(MacAddressCompat::fromBytes)
        if (address != null && address != MacAddressCompat.ANY_ADDRESS) appendLine(makeMacSpan(address.toString()))
    } catch (e: IllegalArgumentException) {
        Timber.w(e)
    } catch (_: SocketException) { }
    if (!macOnly) for (address in interfaceAddresses) {
        append(makeIpSpan(address.address))
        appendLine("/${address.networkPrefixLength}")
    }
}.trimEnd()

private val parseNumericAddress by lazy @SuppressLint("SoonBlockedPrivateApi") {
    InetAddress::class.java.getDeclaredMethod("parseNumericAddress", String::class.java).apply {
        isAccessible = true
    }
}
fun parseNumericAddress(address: String) = if (Build.VERSION.SDK_INT >= 29) {
    InetAddresses.parseNumericAddress(address)
} else parseNumericAddress(null, address) as InetAddress

private val getAllInterfaceNames by lazy { LinkProperties::class.java.getDeclaredMethod("getAllInterfaceNames") }
@Suppress("UNCHECKED_CAST")
val LinkProperties.allInterfaceNames get() = getAllInterfaceNames.invoke(this) as List<String>
private val getAllRoutes by lazy { LinkProperties::class.java.getDeclaredMethod("getAllRoutes") }
@Suppress("UNCHECKED_CAST")
val LinkProperties.allRoutes get() = getAllRoutes.invoke(this) as List<RouteInfo>

fun Context.launchUrl(url: String) {
    if (app.hasTouch) try {
        app.customTabsIntent.launchUrl(this, url.toUri())
        return
    } catch (_: RuntimeException) { }
    SmartSnackbar.make(url).show()
}

fun Context.stopAndUnbind(connection: ServiceConnection) {
    connection.onServiceDisconnected(null)
    unbindService(connection)
}

var MenuItem.isNotGone: Boolean
    get() = isVisible || isEnabled
    set(value) {
        isVisible = value
        isEnabled = value
    }

fun Resources.findIdentifier(name: String, defType: String, defPackage: String, alternativePackage: String? = null) =
    getIdentifier(name, defType, defPackage).let {
        if (alternativePackage != null && it == 0) getIdentifier(name, defType, alternativePackage) else it
    }

@get:RequiresApi(26)
private val newLookup by lazy @TargetApi(26) {
    MethodHandles.Lookup::class.java.getDeclaredConstructor(Class::class.java, Int::class.java).apply {
        isAccessible = true
    }
}

/**
 * Call interface super method.
 *
 * See also: https://stackoverflow.com/a/49532463/2245107
 */
fun InvocationHandler.callSuper(interfaceClass: Class<*>, proxy: Any, method: Method, args: Array<out Any?>?) = when {
    Build.VERSION.SDK_INT >= 26 && method.isDefault -> newLookup.newInstance(interfaceClass, 0xf)   // ALL_MODES
            .`in`(interfaceClass).unreflectSpecial(method, interfaceClass).bindTo(proxy).run {
                if (args == null) invokeWithArguments() else invokeWithArguments(*args)
            }
    // otherwise, we just redispatch it to InvocationHandler
    method.declaringClass.isAssignableFrom(javaClass) -> when {
        method.declaringClass == Object::class.java -> when (method.name) {
            "hashCode" -> System.identityHashCode(proxy)
            "equals" -> proxy === args!![0]
            "toString" -> "${proxy.javaClass.name}@${System.identityHashCode(proxy).toString(16)}"
            else -> error("Unsupported Object method dispatched")
        }
        args == null -> method(this)
        else -> method(this, *args)
    }
    else -> {
        Timber.w("Unhandled method: $method(${args?.contentDeepToString()})")
        null
    }
}

@Suppress("FunctionName")
fun if_nametoindex(ifname: String) = if (Build.VERSION.SDK_INT >= 26) {
    Os.if_nametoindex(ifname)
} else try {
    File("/sys/class/net/$ifname/ifindex").inputStream().bufferedReader().use { it.readLine().trim().toInt() }
} catch (_: FileNotFoundException) {
    NetworkInterface.getByName(ifname)?.index ?: 0
}
