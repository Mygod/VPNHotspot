package be.mygod.vpnhotspot.net.wifi

import android.content.Context
import android.net.wifi.OuiKeyedData
import android.os.PersistableBundle
import android.util.Xml
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import org.xmlpull.v1.XmlPullParser
import org.xmlpull.v1.XmlPullParserException
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.StringReader
import java.nio.charset.StandardCharsets
import java.util.Locale

@RequiresApi(35)
object VendorData {
    private val header = Regex("([0-9a-fA-F]{6}):(.*)")

    fun serialize(input: List<OuiKeyedData>) = input.joinToString("\n") { data ->
        val output = ByteArrayOutputStream()
        data.data.writeToStream(output)
        String.format(Locale.ROOT, "%06x:%s", data.oui, output.toString(StandardCharsets.UTF_8.name()).trimEnd('\n'))
    }

    /**
     * Each entry starts with an OUI header line and runs until the next one, since
     * [PersistableBundle.writeToStream] puts line breaks between tags:
     * https://android.googlesource.com/platform/frameworks/base/+/android-13.0.0_r1/core/java/com/android/internal/util/FastXmlSerializer.java#378
     * [PersistableBundle.readFromStream] cannot detect a truncated entry: KXmlParser reports END_DOCUMENT with the root
     * still open, and restoreFromXml then returns an empty bundle. Hence the entry is first checked for its root element
     * closing:
     * https://android.googlesource.com/platform/libcore/+/android-16.0.0_r1/xml/src/main/java/com/android/org/kxml2/io/KXmlParser.java#377
     * https://android.googlesource.com/platform/frameworks/base/+/android-16.0.0_r1/core/java/android/os/PersistableBundle.java#383
     */
    fun deserialize(input: CharSequence?, context: Context = app): List<OuiKeyedData> {
        val lines = input?.toString()?.lines() ?: return emptyList()
        val result = ArrayList<OuiKeyedData>()
        var index = 0
        while (true) {
            while (index < lines.size && lines[index].isBlank()) ++index
            if (index >= lines.size) return result
            val lineNumber = index + 1
            val match = header.matchEntire(lines[index])
                ?: throw IllegalArgumentException(context.getString(R.string.wifi_vendor_data_error_header, lineNumber))
            val ouiText = match.groupValues[1]
            val oui = ouiText.toInt(16)
            require(oui != 0) { context.getString(R.string.wifi_vendor_data_error_oui_zero, lineNumber) }
            val xml = buildString {
                append(match.groupValues[2])
                while (++index < lines.size && header.matchEntire(lines[index]) == null) append('\n').append(lines[index])
            }
            val incomplete = context.getString(R.string.wifi_vendor_data_error_incomplete_xml, ouiText)
            val data = try {
                val parser = Xml.newPullParser().apply { setInput(StringReader(xml)) }
                var event: Int
                do {
                    event = parser.next()
                } while (event != XmlPullParser.END_DOCUMENT && (event != XmlPullParser.END_TAG || parser.depth != 1))
                require(event == XmlPullParser.END_TAG) { incomplete }
                ByteArrayInputStream(xml.toByteArray(StandardCharsets.UTF_8)).use { PersistableBundle.readFromStream(it) }
            } catch (e: XmlPullParserException) {
                throw IllegalArgumentException(incomplete, e)
            } catch (e: IOException) {
                throw IllegalArgumentException(incomplete, e)
            }
            result += OuiKeyedData.Builder(oui, data).build()
        }
    }
}
