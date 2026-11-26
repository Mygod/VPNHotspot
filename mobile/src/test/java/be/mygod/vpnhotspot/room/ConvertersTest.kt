package be.mygod.vpnhotspot.room

import androidx.compose.runtime.saveable.Saver
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.withStyle
import org.junit.Assert.assertEquals
import org.junit.Test

class ConvertersTest {
    @Test
    fun upgradeAnnotatedStringFormat1RestoresCompose112Ranges() {
        // Rows store Compose's internal marker enum by class and constant name, so resolve it the same way.
        val annotationType = Class.forName("androidx.compose.ui.text.AnnotationType").enumConstants!!
            .associateBy { (it as Enum<*>).name }
        // AnnotatedString.Saver output from Compose 1.12, written out from its Savers.kt: each range is
        // [marker, item, start, end, tag], and SpanStyleSaver saves color, fontSize, fontWeight, fontStyle,
        // fontSynthesis, a -1 fontFamily placeholder, fontFeatureSettings, letterSpacing, baselineShift,
        // textGeometricTransform, localeList, background, textDecoration and shadow, with false for unset values.
        val format1 = arrayListOf("plain bold", arrayListOf(
            arrayListOf(annotationType["Span"],
                arrayListOf(false, false, 700, false, false, -1, null, false, false, false, false, false, false, false),
                6, 10, ""),
            arrayListOf(annotationType["String"], "value", 0, 5, "tag"),
        ))
        val expected = buildAnnotatedString {
            append("plain ")
            withStyle(SpanStyle(fontWeight = FontWeight.Bold)) { append("bold") }
            addStringAnnotation("tag", "value", 0, 5)
        }
        @Suppress("UNCHECKED_CAST")
        val saver = AnnotatedString.Saver as Saver<AnnotatedString, Any>

        assertEquals(expected, with(saver) { restore(Converters.upgradeAnnotatedStringFormat1(format1)) })
    }
}
