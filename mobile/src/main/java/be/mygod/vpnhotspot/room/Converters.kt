package be.mygod.vpnhotspot.room

import android.text.TextUtils
import androidx.room.TypeConverter
import be.mygod.vpnhotspot.util.useParcel
import timber.log.Timber
import java.net.InetAddress
import java.nio.ByteBuffer
import java.nio.ByteOrder

object Converters {
    @JvmStatic
    @TypeConverter
    fun persistCharSequence(cs: CharSequence) = useParcel { p ->
        TextUtils.writeToParcel(cs, p, 0)
        p.marshall()
    }

    @JvmStatic
    @TypeConverter
    fun unpersistCharSequence(data: ByteArray) = useParcel { p ->
        p.unmarshall(data, 0, data.size)
        p.setDataPosition(0)
        try {
            TextUtils.CHAR_SEQUENCE_CREATOR.createFromParcel(p)
        } catch (e: RuntimeException) {
            Timber.w(e)
            ""
        }
    }

    @JvmStatic
    @TypeConverter
    fun persistInetAddress(address: InetAddress): ByteArray = address.address

    @JvmStatic
    @TypeConverter
    fun unpersistInetAddress(data: ByteArray): InetAddress = InetAddress.getByAddress(data)
}

fun String.macToLong(): Long = ByteBuffer.allocate(8).run {
    order(ByteOrder.LITTLE_ENDIAN)
    mark()
    put(split(':').map { Integer.parseInt(it, 16).toByte() }.toByteArray())
    reset()
    long
}

fun Iterable<Byte>.macToString() = joinToString(":") { "%02x".format(it) }
fun Long.macToString(): String = ByteBuffer.allocate(8).run {
    order(ByteOrder.LITTLE_ENDIAN)
    putLong(this@macToString)
    array().take(6).macToString()
}
