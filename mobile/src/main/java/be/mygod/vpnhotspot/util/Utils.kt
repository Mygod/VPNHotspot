package be.mygod.vpnhotspot.util

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.ServiceConnection
import android.content.res.Resources
import android.net.LinkProperties
import android.net.MacAddress
import android.net.NetworkRequest
import android.net.RouteInfo
import android.net.http.ConnectionMigrationOptions
import android.net.http.HttpEngine
import android.os.Build
import android.os.Parcel
import android.os.Parcelable
import android.os.RemoteException
import android.os.ext.SdkExtensions
import android.text.Spannable
import android.text.SpannableString
import android.text.SpannableStringBuilder
import android.view.MenuItem
import androidx.annotation.RequiresExtension
import androidx.core.i18n.DateTimeFormatter
import androidx.core.i18n.DateTimeFormatterSkeletonOptions
import androidx.core.net.toUri
import androidx.core.os.ParcelCompat
import androidx.fragment.app.DialogFragment
import androidx.fragment.app.FragmentManager
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.InternalCoroutinesApi
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.job
import kotlinx.coroutines.withContext
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
import java.util.concurrent.Executor

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

inline fun <T> useParcel(block: (Parcel) -> T): T {
    val parcel = Parcel.obtain()
    try {
        return block(parcel)
    } finally {
        parcel.recycle()
    }
}

fun Parcelable?.toByteArray(parcelableFlags: Int = 0) = useParcel { parcel ->
    parcel.writeParcelable(this, parcelableFlags)
    parcel.marshall()
}

fun <T : Parcelable> ByteArray.toParcelable(classLoader: ClassLoader?, clazz: Class<T>) = useParcel { parcel ->
    parcel.unmarshall(this, 0, size)
    parcel.setDataPosition(0)
    ParcelCompat.readParcelable(parcel, classLoader, clazz)
}
inline fun <reified T : Parcelable> ByteArray.toParcelable(classLoader: ClassLoader?) =
    toParcelable(classLoader, T::class.java)

fun Context.ensureReceiverUnregistered(receiver: BroadcastReceiver) {
    try {
        unregisterReceiver(receiver)
    } catch (_: IllegalArgumentException) { }
}

private val dateTimeFormat = DateTimeFormatterSkeletonOptions.Builder(
    year = DateTimeFormatterSkeletonOptions.Year.NUMERIC,
    month = DateTimeFormatterSkeletonOptions.Month.NUMERIC,
    day = DateTimeFormatterSkeletonOptions.Day.NUMERIC,
    period = DateTimeFormatterSkeletonOptions.Period.ABBREVIATED,
    hour = DateTimeFormatterSkeletonOptions.Hour.NUMERIC,
    minute = DateTimeFormatterSkeletonOptions.Minute.NUMERIC,
    second = DateTimeFormatterSkeletonOptions.Second.NUMERIC,
    fractionalSecond = DateTimeFormatterSkeletonOptions.FractionalSecond.NUMERIC_3_DIGITS,
).build()
fun Context.formatTimestamp(timestamp: Long) = DateTimeFormatter(this, dateTimeFormat,
    resources.configuration.locales[0]).format(timestamp)

fun DialogFragment.showAllowingStateLoss(manager: FragmentManager, tag: String? = null) {
    if (!manager.isStateSaved) show(manager, tag)
}

fun broadcastReceiver(receiver: (Context, Intent) -> Unit) = object : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) = receiver(context, intent)
}

fun intentFilter(vararg actions: String) = IntentFilter().also { actions.forEach(it::addAction) }

fun <T> Iterable<T>.joinToSpanned(separator: CharSequence = ", ", prefix: CharSequence = "", postfix: CharSequence = "",
                                  limit: Int = -1, truncated: CharSequence = "...",
                                  transform: ((T) -> CharSequence)? = null) =
    joinTo(SpannableStringBuilder(), separator, prefix, postfix, limit, truncated, transform)
fun <T> Sequence<T>.joinToSpanned(separator: CharSequence = ", ", prefix: CharSequence = "", postfix: CharSequence = "",
                                  limit: Int = -1, truncated: CharSequence = "...",
                                  transform: ((T) -> CharSequence)? = null) =
    joinTo(SpannableStringBuilder(), separator, prefix, postfix, limit, truncated, transform)

private fun ByteArray.u32(index: Int) =
    (this[index].toInt() shl 24 or
            ((this[index + 1].toInt() and 0xFF) shl 16) or
            ((this[index + 2].toInt() and 0xFF) shl 8) or
            (this[index + 3].toInt() and 0xFF)).toUInt()

val InetAddress.isBogon get() = address.let { bytes ->
    when (bytes.size) {
        4 -> when (bytes[0].toInt()) {
            0, 10, 127 -> true
            else -> bytes.u32(0).let { address ->
                when {
                    (address and 0xFFC00000u) == 0x64400000u -> true
                    (address and 0xFFFF0000u) == 0xA9FE0000u -> true
                    (address and 0xFFF00000u) == 0xAC100000u -> true
                    (address and 0xFFFFFF00u) == 0xC0000000u ->
                        address != 0xC0000009u && address != 0xC000000Au
                    (address and 0xFFFFFF00u) == 0xC0000200u -> true
                    address == 0xC0586302u -> true
                    (address and 0xFFFF0000u) == 0xC0A80000u -> true
                    (address and 0xFFFE0000u) == 0xC6120000u -> true
                    (address and 0xFFFFFF00u) == 0xC6336400u -> true
                    (address and 0xFFFFFF00u) == 0xCB007100u -> true
                    (address and 0xE0000000u) == 0xE0000000u -> true
                    else -> false
                }
            }
        }
        16 -> {
            val first = bytes.u32(0)
            val second = bytes.u32(4)
            val third = bytes.u32(8)
            val fourth = bytes.u32(12)
            when {
                first == 0u && second == 0u && third == 0u && (fourth == 0u || fourth == 1u) -> true
                first == 0u && second == 0u && third == 0x0000FFFFu -> true
                first == 0x0064FF9Bu && (second and 0xFFFF0000u) == 0x00010000u -> true
                first == 0x01000000u && (second == 0u || second == 1u) -> true
                (first and 0xFFFFFE00u) == 0x20010000u -> !(
                        first == 0x20010001u && second == 0u && third == 0u && fourth in 1u..3u ||
                        first == 0x20010003u ||
                        first == 0x20010004u && (second and 0xFFFF0000u) == 0x01120000u ||
                        (first and 0xFFFFFFF0u) == 0x20010020u ||
                        (first and 0xFFFFFFF0u) == 0x20010030u)
                first == 0x20010DB8u -> true
                (first and 0xFFFF0000u) == 0x20020000u -> true
                (first and 0xFFFFF000u) == 0x3FFF0000u -> true
                (first and 0xFFFF0000u) == 0x5F000000u -> true
                (first and 0xFE000000u) == 0xFC000000u -> true
                (first and 0xFFC00000u) == 0xFE800000u ||
                        (first and 0xFFC00000u) == 0xFEC00000u -> true
                (first and 0xFF000000u) == 0xFF000000u -> true
                else -> false
            }
        }
        else -> false
    }
}

fun makeIpSpan(ip: InetAddress) = ip.hostAddress.let {
    if (!app.hasTouch || ip.isBogon) it else SpannableString(it).apply {
        setSpan(CustomTabsUrlSpan("https://ipinfo.io/$it"), 0, length, Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
    }
}
fun makeMacSpan(mac: String) = if (app.hasTouch) SpannableString(mac).apply {
    setSpan(CustomTabsUrlSpan("https://macaddress.io/macaddress/$mac"), 0, length,
        Spannable.SPAN_EXCLUSIVE_EXCLUSIVE)
} else mac

fun NetworkInterface?.formatAddresses(macOnly: Boolean = false,
                                      macOverride: MacAddress? = null) = SpannableStringBuilder().apply {
    var address = macOverride
    if (address == null && this@formatAddresses != null) try {
        val hardwareAddress = hardwareAddress
        address = try {
            hardwareAddress?.let(MacAddress::fromBytes)
        } catch (e: IllegalArgumentException) {
            try {
                hardwareAddress?.let { MacAddress.fromString(String(it)) }.also { Timber.d(e) }
            } catch (e2: IllegalArgumentException) {
                e.addSuppressed(e2)
                Timber.w(e)
                null
            }
        }
    } catch (_: SocketException) { }
    if (address != null && address != MacAddressCompat.ANY_ADDRESS) appendLine(makeMacSpan(address.toString()))
    if (!macOnly && this@formatAddresses != null) for (address in interfaceAddresses) {
        append(makeIpSpan(address.address))
        address.networkPrefixLength.also { if (it.toInt() != address.address.address.size * 8) append("/$it") }
        appendLine()
    }
}.trimEnd()

private val getAllInterfaceNames by lazy { LinkProperties::class.java.getDeclaredMethod("getAllInterfaceNames") }
@Suppress("UNCHECKED_CAST")
val LinkProperties.allInterfaceNames get() = getAllInterfaceNames(this) as List<String>
private val getAllRoutes by lazy { LinkProperties::class.java.getDeclaredMethod("getAllRoutes") }
@Suppress("UNCHECKED_CAST")
val LinkProperties.allRoutes get() = getAllRoutes(this) as List<RouteInfo>

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
fun Class<*>.privateLookup() = if (Build.VERSION.SDK_INT < 33) try {
    newLookup.newInstance(this, 0xf)    // ALL_MODES
} catch (e: ReflectiveOperationException) {
    Timber.w(e)
    MethodHandles.lookup().`in`(this)
} else MethodHandles.privateLookupIn(this, null)

/**
 * Call interface super method.
 *
 * See also: https://stackoverflow.com/a/49532463/2245107
 */
fun InvocationHandler.callSuper(interfaceClass: Class<*>, proxy: Any, method: Method, args: Array<out Any?>?) = when {
    method.isDefault -> interfaceClass.privateLookup().unreflectSpecial(method, interfaceClass).bindTo(proxy).run {
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
        addQuicHint("macaddress.io", 443, 443)
    }.build()
}
suspend fun <T> connectCancellable(url: String, block: suspend (HttpURLConnection) -> T): T {
    val conn = (if (Build.VERSION.SDK_INT >= 34 || Build.VERSION.SDK_INT >= Build.VERSION_CODES.S &&
        SdkExtensions.getExtensionVersion(Build.VERSION_CODES.S) >= 7) {
        engine.openConnection(URL(url))
    } else @Suppress("BlockingMethodInNonBlockingContext") URL(url).openConnection()) as HttpURLConnection
    return coroutineScope {
        @OptIn(InternalCoroutinesApi::class)    // https://github.com/Kotlin/kotlinx.coroutines/issues/4117
        coroutineContext.job.invokeOnCompletion(true) { conn.disconnect() }
        try {
            withContext(Dispatchers.IO) { block(conn) }
        } finally {
            conn.disconnect()
        }
    }
}

object InPlaceExecutor : Executor {
    override fun execute(command: Runnable) = try {
        command.run()
    } catch (e: Exception) {
        Timber.w(e) // prevent Binder stub swallowing the exception
    }
}
