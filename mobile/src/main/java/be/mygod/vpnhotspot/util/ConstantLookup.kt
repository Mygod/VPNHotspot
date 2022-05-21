package be.mygod.vpnhotspot.util

import android.os.Build
import androidx.collection.LongSparseArray
import androidx.collection.SparseArrayCompat
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import timber.log.Timber

class ConstantLookup(private val prefix: String, private val lookup29: Array<out String?>,
                     private val clazz: () -> Class<*>) {
    private val lookup by lazy {
        SparseArrayCompat<String>().apply {
            for (field in clazz().declaredFields) try {
                if (field?.type == Int::class.java && field.name.startsWith(prefix)) put(field.getInt(null), field.name)
            } catch (e: Exception) {
                Timber.w(e)
            }
        }
    }

    operator fun invoke(reason: Int, trimPrefix: Boolean = false): String {
        if (Build.VERSION.SDK_INT >= 30) try {
            lookup.get(reason)?.let { return if (trimPrefix) it.substring(prefix.length) else it }
        } catch (e: ReflectiveOperationException) {
            Timber.w(e)
        }
        return lookup29.getOrNull(reason)?.let { if (trimPrefix) it else prefix + it }
                ?: app.getString(R.string.failure_reason_unknown, reason)
    }
}

@Suppress("FunctionName")
fun ConstantLookup(prefix: String, vararg lookup29: String?, clazz: () -> Class<*>) =
        ConstantLookup(prefix, lookup29, clazz)
@Suppress("FunctionName")
inline fun <reified T> ConstantLookup(prefix: String, vararg lookup29: String?) =
        ConstantLookup(prefix, lookup29) { T::class.java }

class LongConstantLookup(private val clazz: Class<*>, private val prefix: String) {
    private val lookup = LongSparseArray<String>().apply {
        for (field in clazz.declaredFields) try {
            if (field.type == Long::class.java && field.name.startsWith(prefix)) put(field.getLong(null), field.name)
        } catch (e: Exception) {
            Timber.w(e)
        }
    }

    operator fun invoke(reason: Long, trimPrefix: Boolean = false): String {
        try {
            lookup.get(reason)?.let { return if (trimPrefix) it.substring(prefix.length) else it }
        } catch (e: ReflectiveOperationException) {
            Timber.w(e)
        }
        return app.getString(R.string.failure_reason_unknown, reason)
    }
}
