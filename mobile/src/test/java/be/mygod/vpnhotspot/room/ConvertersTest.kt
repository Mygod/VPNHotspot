package be.mygod.vpnhotspot.room

import androidx.compose.runtime.saveable.Saver
import androidx.compose.runtime.saveable.SaverScope
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.withStyle
import org.junit.Assert.assertEquals
import org.junit.Test

class ConvertersTest {
    @Test
    fun upgradeAnnotatedStringFormat1RestoresStyledRanges() {
        val text = buildAnnotatedString {
            append("plain ")
            withStyle(SpanStyle(fontWeight = FontWeight.Bold)) { append("bold") }
            addStringAnnotation("tag", "value", 0, 5)
        }
        @Suppress("UNCHECKED_CAST")
        val saver = AnnotatedString.Saver as Saver<AnnotatedString, Any>
        val (string, ranges) = with(saver) { SaverScope { true }.save(text) } as List<*>
        // Compose 1.12 saved each range's [marker, item] pair inline: [marker, item, start, end, tag].
        val format1 = arrayListOf(string, (ranges as List<*>).map { range ->
            val (annotation, start, end, tag) = range as List<*>
            annotation as List<*> + listOf(start, end, tag)
        })

        assertEquals(text, with(saver) { restore(Converters.upgradeAnnotatedStringFormat1(format1)) })
    }
}
