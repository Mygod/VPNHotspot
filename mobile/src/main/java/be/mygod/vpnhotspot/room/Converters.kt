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
    /** [AnnotatedString.Saver] output from Compose 1.12 and earlier. */
    private const val ANNOTATED_STRING_FORMAT_1 = 0x41535401
    /**
     * [AnnotatedString.Saver] output from Compose 1.13, which saves each range as `[[marker, item], start, end, tag]`
     * instead of `[marker, item, start, end, tag]`.
     */
    private const val ANNOTATED_STRING_FORMAT = 0x41535402
    @Suppress("UNCHECKED_CAST")
    private val annotatedStringSaver = AnnotatedString.Saver as Saver<AnnotatedString, Any>

    /** Reshapes the ranges of [ANNOTATED_STRING_FORMAT_1] into the [ANNOTATED_STRING_FORMAT] layout. */
    internal fun upgradeAnnotatedStringFormat1(saved: Any): Any {
        val (text, ranges) = saved as List<*>
        if (ranges !is List<*>) return saved
        return arrayListOf(text, ranges.mapTo(ArrayList()) { range ->
            val (marker, item, start, end, tag) = range as List<*>
            arrayListOf(arrayListOf(marker, item), start, end, tag)
        })
    }

    @JvmStatic
    @TypeConverter
    fun persistAnnotatedString(text: AnnotatedString) = useParcel { p ->
        val saved = with(annotatedStringSaver) { SaverScope { true }.save(text) }
        // Both formats are identical without ranges, so such rows stay readable by builds that only know format 1.
        val ranges = (saved as List<*>)[1]
        p.writeInt(if (ranges is List<*> && ranges.isNotEmpty()) ANNOTATED_STRING_FORMAT else ANNOTATED_STRING_FORMAT_1)
        p.writeValue(saved)
        p.marshall()
    }

    @JvmStatic
    @TypeConverter
    fun unpersistAnnotatedString(data: ByteArray?) = data?.let {
        useParcel { p ->
            p.unmarshall(data, 0, data.size)
            p.setDataPosition(0)
            try {
                val format = p.readInt()
                if (format == ANNOTATED_STRING_FORMAT || format == ANNOTATED_STRING_FORMAT_1) {
                    @Suppress("DEPRECATION")
                    val saved = p.readValue(Converters::class.java.classLoader) ?: return@useParcel null
                    return@useParcel with(annotatedStringSaver) {
                        restore(if (format == ANNOTATED_STRING_FORMAT_1) upgradeAnnotatedStringFormat1(saved) else saved)
                    }
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
