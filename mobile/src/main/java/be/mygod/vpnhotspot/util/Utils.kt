package be.mygod.vpnhotspot.util

import android.annotation.SuppressLint
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.ServiceConnection
import android.content.res.Resources
import android.net.InetAddresses
import android.net.LinkProperties
import android.net.MacAddress
import android.net.NetworkRequest
import android.net.RouteInfo
import android.net.http.ConnectionMigrationOptions
import android.net.http.HttpEngine
import android.os.Build
import android.os.RemoteException
import android.os.ext.SdkExtensions
import android.text.Spannable
import android.text.SpannableString
import android.text.SpannableStringBuilder
import android.text.Spanned
import android.view.MenuItem
import android.view.View
import android.widget.ImageView
import androidx.annotation.DrawableRes
import androidx.annotation.RequiresExtension
import androidx.core.net.toUri
import androidx.core.view.isVisible
import androidx.databinding.BindingAdapter
import androidx.fragment.app.DialogFragment
import androidx.fragment.app.FragmentManager
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.suspendCancellableCoroutine
import timber.log.Timber
import java.io.File
import java.lang.invoke.MethodHandles
import java.lang.reflect.InvocationHandler
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Method
import java.net.HttpURLConnection
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketException
import java.net.URL
import java.util.Locale
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

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

fun Method.matches(name: String, vararg classes: Class<*>) = this.name == name && parameterCount == classes.size &&
        classes.indices.all { i -> parameters[i].type == classes[i] }
inline fun <reified T> Method.matches1(name: String) = matches(name, T::class.java)

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

private val formatSequence = "%([0-9]+\\$|<?)([^a-zA-z%]*)([[a-zA-Z%]&&[^tT]]|[tT][a-zA-Z])".toPattern()
/**
 * Version of [String.format] that works on [Spanned] strings to preserve rich text formatting.
 * Both the `format` as well as any `%s args` can be Spanned and will have their formatting preserved.
 * Due to the way [Spannable]s work, any argument's spans will can only be included **once** in the result.
 * Any duplicates will appear as text only.
 *
 * See also: https://github.com/george-steel/android-utils/blob/289aff11e53593a55d780f9f5986e49343a79e55/src/org/oshkimaadziig/george/androidutils/SpanFormatter.java
 *
 * @param locale
 * the locale to apply; `null` value means no localization.
 * @param args
 * the list of arguments passed to the formatter.
 * @return the formatted string (with spans).
 * @see String.format
 * @author George T. Steel
 */
fun CharSequence.format(locale: Locale, vararg args: Any) = SpannableStringBuilder(this).apply {
    var i = 0
    var argAt = -1
    while (i < length) {
        val m = formatSequence.matcher(this)
        if (!m.find(i)) break
        i = m.start()
        val exprEnd = m.end()
        val argTerm = m.group(1)!!
        val modTerm = m.group(2)
        val cookedArg = when (val typeTerm = m.group(3)) {
            "%" -> "%"
            "n" -> "\n"
            else -> {
                val argItem = args[when (argTerm) {
                    "" -> ++argAt
                    "<" -> argAt
                    else -> Integer.parseInt(argTerm.substring(0, argTerm.length - 1)) - 1
                }]
                if (typeTerm == "s" && argItem is Spanned) argItem else {
                    String.format(locale, "%$modTerm$typeTerm", argItem)
                }
            }
        }
        replace(i, exprEnd, cookedArg)
        i += cookedArg.length
    }
}

fun <T> Iterable<T>.joinToSpanned(separator: CharSequence = ", ", prefix: CharSequence = "", postfix: CharSequence = "",
                                  limit: Int = -1, truncated: CharSequence = "...",
                                  transform: ((T) -> CharSequence)? = null) =
    joinTo(SpannableStringBuilder(), separator, prefix, postfix, limit, truncated, transform)
fun <T> Sequence<T>.joinToSpanned(separator: CharSequence = ", ", prefix: CharSequence = "", postfix: CharSequence = "",
                                  limit: Int = -1, truncated: CharSequence = "...",
                                  transform: ((T) -> CharSequence)? = null) =
    joinTo(SpannableStringBuilder(), separator, prefix, postfix, limit, truncated, transform)

fun makeIpSpan(ip: InetAddress) = ip.hostAddress.let {
    // exclude all bogon IP addresses supported by Android APIs
    if (!app.hasTouch || ip.isMulticastAddress || ip.isAnyLocalAddress || ip.isLoopbackAddress ||
            ip.isLinkLocalAddress || ip.isSiteLocalAddress || ip.isMCGlobal || ip.isMCNodeLocal ||
            ip.isMCLinkLocal || ip.isMCSiteLocal || ip.isMCOrgLocal) it else SpannableString(it).apply {
        setSpan(CustomTabsUrlSpan("https://ipinfo.io/$it"), 0, length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
    }
}
fun makeMacSpan(mac: String) = if (app.hasTouch) SpannableString(mac).apply {
    setSpan(CustomTabsUrlSpan("https://maclookup.app/search/result?mac=$mac"), 0, length,
        Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
} else mac

fun NetworkInterface.formatAddresses(macOnly: Boolean = false) = SpannableStringBuilder().apply {
    try {
        val hardwareAddress = hardwareAddress
        val address = try {
            hardwareAddress?.let(MacAddress::fromBytes)
        } catch (e: IllegalArgumentException) {
            try {
                hardwareAddress?.let { MacAddress.fromString(String(it)) }.also { Timber.d(e) }
            } catch (e2: IllegalArgumentException) {
                e.addSuppressed(e2)
                Timber.w(e)
            }
        }
        if (address != null && address != MacAddressCompat.ANY_ADDRESS) appendLine(makeMacSpan(address.toString()))
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

private val newLookup by lazy {
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
    method.isDefault -> try {
        newLookup.newInstance(interfaceClass, 0xf)   // ALL_MODES
    } catch (e: ReflectiveOperationException) {
        Timber.w(e)
        MethodHandles.lookup().`in`(interfaceClass)
    }.unreflectSpecial(method, interfaceClass).bindTo(proxy).run {
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

fun globalNetworkRequestBuilder() = NetworkRequest.Builder().apply {
    if (Build.VERSION.SDK_INT >= 31) setIncludeOtherUidNetworks(true)
}

@get:RequiresExtension(Build.VERSION_CODES.S, 7)
private val engine by lazy @RequiresExtension(Build.VERSION_CODES.S, 7) {
    val cache = File(app.deviceStorage.cacheDir, "httpEngine")
    HttpEngine.Builder(app.deviceStorage).apply {
        if (cache.mkdirs() || cache.isDirectory) {
            setStoragePath(cache.absolutePath)
            setEnableHttpCache(HttpEngine.Builder.HTTP_CACHE_DISK, 1024 * 1024)
        }
        setConnectionMigrationOptions(ConnectionMigrationOptions.Builder().apply {
            setDefaultNetworkMigration(ConnectionMigrationOptions.MIGRATION_OPTION_ENABLED)
            setPathDegradationMigration(ConnectionMigrationOptions.MIGRATION_OPTION_ENABLED)
        }.build())
        setEnableBrotli(true)
    }.build()
}
suspend fun <T> connectCancellable(url: String, block: suspend (HttpURLConnection) -> T): T {
    val conn = (if (Build.VERSION.SDK_INT >= 34 || Build.VERSION.SDK_INT >= Build.VERSION_CODES.S &&
        SdkExtensions.getExtensionVersion(Build.VERSION_CODES.S) >= 7) {
        engine.openConnection(URL(url))
    } else @Suppress("BlockingMethodInNonBlockingContext") URL(url).openConnection()) as HttpURLConnection
    return suspendCancellableCoroutine { cont ->
        val job = GlobalScope.launch(Dispatchers.IO) {
            try {
                cont.resume(block(conn))
            } catch (e: Throwable) {
                cont.resumeWithException(e)
            } finally {
                conn.disconnect()
            }
        }
        cont.invokeOnCancellation {
            job.cancel(it as? CancellationException)
            conn.disconnect()
        }
    }
}
