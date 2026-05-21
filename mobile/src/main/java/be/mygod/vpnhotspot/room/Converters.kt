package be.mygod.vpnhotspot.room

import android.net.MacAddress
import android.text.Html
import android.text.Spanned
import android.text.TextUtils
import androidx.compose.runtime.saveable.Saver
import androidx.compose.runtime.saveable.SaverScope
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.fromHtml
import androidx.room.TypeConverter
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.net.MacAddressCompat.Companion.toLong
import be.mygod.vpnhotspot.util.useParcel
import timber.log.Timber
import java.net.InetAddress

object Converters {
    private const val ANNOTATED_STRING_FORMAT = 0x41535401
    @Suppress("UNCHECKED_CAST")
    private val annotatedStringSaver = AnnotatedString.Saver as Saver<AnnotatedString, Any>
    private val saverScope = SaverScope { true }

    @JvmStatic
    @TypeConverter
    fun persistAnnotatedString(text: AnnotatedString) = useParcel { p ->
        p.writeInt(ANNOTATED_STRING_FORMAT)
        p.writeValue(with(annotatedStringSaver) { saverScope.save(text) })
        p.marshall()
    }

    @JvmStatic
    @TypeConverter
    fun unpersistAnnotatedString(data: ByteArray?) = data?.let {
        useParcel { p ->
            p.unmarshall(data, 0, data.size)
            p.setDataPosition(0)
            try {
                if (p.readInt() == ANNOTATED_STRING_FORMAT) {
                    @Suppress("DEPRECATION")
                    val saved = p.readValue(Converters::class.java.classLoader) ?: return@useParcel null
                    return@useParcel with(annotatedStringSaver) { restore(saved) }
                }
            } catch (e: RuntimeException) {
                Timber.w(e)
                return@useParcel null
            }
            p.setDataPosition(0)
            try {
                val legacy = TextUtils.CHAR_SEQUENCE_CREATOR.createFromParcel(p) ?: return@useParcel null
                if (legacy is Spanned) AnnotatedString.fromHtml(Html.toHtml(legacy, Html.TO_HTML_PARAGRAPH_LINES_CONSECUTIVE))
                else AnnotatedString(legacy.toString())
            } catch (e: RuntimeException) {
                Timber.w(e)
                null
            }
        }
    }

    @JvmStatic
    @TypeConverter
    fun persistMacAddress(address: MacAddress) = address.toLong()

    @JvmStatic
    @TypeConverter
    fun unpersistMacAddress(address: Long) = MacAddressCompat(address).toPlatform()

    @JvmStatic
    @TypeConverter
    fun persistInetAddress(address: InetAddress): ByteArray = address.address

    @JvmStatic
    @TypeConverter
    fun unpersistInetAddress(data: ByteArray): InetAddress = InetAddress.getByAddress(data)
}
