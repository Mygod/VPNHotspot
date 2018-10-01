package be.mygod.vpnhotspot.room

import android.os.Parcel
import android.text.TextUtils
import androidx.room.TypeConverter

class Converters {
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
}
