package be.mygod.vpnhotspot.room

import android.os.Parcel
import android.text.TextUtils
import androidx.room.TypeConverter
import java.net.InetAddress
import java.nio.ByteBuffer
import java.nio.ByteOrder

object Converters {
    @JvmStatic
    @TypeConverter
    fun persistCharSequence(cs: CharSequence): ByteArray {
        val p = Parcel.obtain()
        try {
            TextUtils.writeToParcel(cs, p, 0)
            return p.marshall()
        } finally {
            p.recycle()
        }
    }

    @JvmStatic
    @TypeConverter
    fun unpersistCharSequence(data: ByteArray): CharSequence {
        val p = Parcel.obtain()
        try {
            p.unmarshall(data, 0, data.size)
            p.setDataPosition(0)
            return TextUtils.CHAR_SEQUENCE_CREATOR.createFromParcel(p)
        } finally {
            p.recycle()
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

fun Long.macToString(): String = ByteBuffer.allocate(8).run {
    order(ByteOrder.LITTLE_ENDIAN)
    putLong(this@macToString)
    array().take(6).joinToString(":") { "%02x".format(it) }
}
