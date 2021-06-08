@file:JvmName("Utils")

package be.mygod.librootkotlinx

import android.annotation.SuppressLint
import android.content.Context
import android.os.Parcel
import android.os.Parcelable
import android.system.ErrnoException
import android.system.OsConstants
import android.util.*
import androidx.annotation.RequiresApi
import kotlinx.parcelize.Parcelize
import java.io.IOException
import java.util.*

class NoShellException(cause: Throwable) : Exception("Root missing", cause)

internal val currentInstructionSet by lazy {
    val classVMRuntime = Class.forName("dalvik.system.VMRuntime")
    val runtime = classVMRuntime.getDeclaredMethod("getRuntime").invoke(null)
    classVMRuntime.getDeclaredMethod("getCurrentInstructionSet").invoke(runtime) as String
}

private val classSystemProperties by lazy { Class.forName("android.os.SystemProperties") }
@get:RequiresApi(26)
internal val isVndkLite by lazy {
    classSystemProperties.getDeclaredMethod("getBoolean", String::class.java, Boolean::class.java).invoke(null,
            "ro.vndk.lite", false) as Boolean
}
@get:RequiresApi(26)
internal val vndkVersion by lazy {
    classSystemProperties.getDeclaredMethod("get", String::class.java, String::class.java).invoke(null,
            "ro.vndk.version", "") as String
}

val systemContext by lazy {
    val classActivityThread = Class.forName("android.app.ActivityThread")
    val activityThread = classActivityThread.getMethod("systemMain").invoke(null)
    classActivityThread.getMethod("getSystemContext").invoke(activityThread) as Context
}

@Parcelize
data class ParcelableByte(val value: Byte) : Parcelable

@Parcelize
data class ParcelableShort(val value: Short) : Parcelable

@Parcelize
data class ParcelableInt(val value: Int) : Parcelable

@Parcelize
data class ParcelableLong(val value: Long) : Parcelable

@Parcelize
data class ParcelableFloat(val value: Float) : Parcelable

@Parcelize
data class ParcelableDouble(val value: Double) : Parcelable

@Parcelize
data class ParcelableBoolean(val value: Boolean) : Parcelable

@Parcelize
data class ParcelableString(val value: String) : Parcelable

@Parcelize
data class ParcelableByteArray(val value: ByteArray) : Parcelable {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false

        other as ParcelableByteArray

        if (!value.contentEquals(other.value)) return false

        return true
    }

    override fun hashCode(): Int {
        return value.contentHashCode()
    }
}

@Parcelize
data class ParcelableIntArray(val value: IntArray) : Parcelable {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false

        other as ParcelableIntArray

        if (!value.contentEquals(other.value)) return false

        return true
    }

    override fun hashCode(): Int {
        return value.contentHashCode()
    }
}

@Parcelize
data class ParcelableLongArray(val value: LongArray) : Parcelable {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false

        other as ParcelableLongArray

        if (!value.contentEquals(other.value)) return false

        return true
    }

    override fun hashCode(): Int {
        return value.contentHashCode()
    }
}

@Parcelize
data class ParcelableFloatArray(val value: FloatArray) : Parcelable {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false

        other as ParcelableFloatArray

        if (!value.contentEquals(other.value)) return false

        return true
    }

    override fun hashCode(): Int {
        return value.contentHashCode()
    }
}

@Parcelize
data class ParcelableDoubleArray(val value: DoubleArray) : Parcelable {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false

        other as ParcelableDoubleArray

        if (!value.contentEquals(other.value)) return false

        return true
    }

    override fun hashCode(): Int {
        return value.contentHashCode()
    }
}

@Parcelize
data class ParcelableBooleanArray(val value: BooleanArray) : Parcelable {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false

        other as ParcelableBooleanArray

        if (!value.contentEquals(other.value)) return false

        return true
    }

    override fun hashCode(): Int {
        return value.contentHashCode()
    }
}

@Parcelize
data class ParcelableStringArray(val value: Array<String>) : Parcelable {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false

        other as ParcelableStringArray

        if (!value.contentEquals(other.value)) return false

        return true
    }

    override fun hashCode(): Int {
        return value.contentHashCode()
    }
}

@Parcelize
data class ParcelableStringList(val value: List<String>) : Parcelable

@Parcelize
data class ParcelableSparseIntArray(val value: SparseIntArray) : Parcelable

@Parcelize
data class ParcelableSparseLongArray(val value: SparseLongArray) : Parcelable

@Parcelize
data class ParcelableSparseBooleanArray(val value: SparseBooleanArray) : Parcelable

@Parcelize
data class ParcelableCharSequence(val value: CharSequence) : Parcelable

@Parcelize
data class ParcelableSize(val value: Size) : Parcelable

@Parcelize
data class ParcelableSizeF(val value: SizeF) : Parcelable

@Parcelize
data class ParcelableArray(val value: Array<Parcelable?>) : Parcelable {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false

        other as ParcelableArray

        if (!value.contentEquals(other.value)) return false

        return true
    }

    override fun hashCode(): Int {
        return value.contentHashCode()
    }
}

@Parcelize
data class ParcelableList(val value: List<Parcelable?>) : Parcelable

@SuppressLint("Recycle")
inline fun <T> useParcel(block: (Parcel) -> T) = Parcel.obtain().run {
    try {
        block(this)
    } finally {
        recycle()
    }
}

fun Parcelable?.toByteArray(parcelableFlags: Int = 0) = useParcel { p ->
    p.writeParcelable(this, parcelableFlags)
    p.marshall()
}
inline fun <reified T : Parcelable> ByteArray.toParcelable(classLoader: ClassLoader? = T::class.java.classLoader) =
    useParcel { p ->
        p.unmarshall(this, 0, size)
        p.setDataPosition(0)
        p.readParcelable<T>(classLoader)
    }

// Stream closed caused in NullOutputStream
val IOException.isEBADF get() = (cause as? ErrnoException)?.errno == OsConstants.EBADF ||
        message?.lowercase(Locale.ENGLISH) == "stream closed"
