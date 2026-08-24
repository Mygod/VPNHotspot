package be.mygod.vpnhotspot.room

import android.text.SpannableString
import android.text.Spanned
import android.text.TextUtils
import android.text.style.URLSpan
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import be.mygod.vpnhotspot.util.useParcel
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Test

class ConvertersTest {
    @Test
    fun annotatedStringRoundTripUsesComposeSaver() {
        val expected = buildAnnotatedString {
            append("client link")
            addStyle(SpanStyle(fontWeight = FontWeight.Bold), 0, 6)
            addLink(
                LinkAnnotation.Url(
                    "https://example.com/client",
                    TextLinkStyles(style = SpanStyle(color = Color.Red, textDecoration = TextDecoration.Underline)),
                ),
                7,
                11,
            )
        }

        assertEquals(expected, Converters.unpersistAnnotatedString(Converters.persistAnnotatedString(expected)))
    }

    @Test
    fun legacyTextUtilsNicknameStillReads() {
        val expectedUrl = "https://example.com/client"
        val legacy = SpannableString("client").apply {
            setSpan(URLSpan(expectedUrl), 0, length, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
        }
        val data = useParcel { parcel ->
            TextUtils.writeToParcel(legacy, parcel, 0)
            parcel.marshall()
        }

        val actual = Converters.unpersistAnnotatedString(data)

        assertNotNull(actual)
        val restored = actual as AnnotatedString
        assertEquals("client", restored.text)
        val link = restored.getLinkAnnotations(0, restored.length).single()
        assertEquals(0, link.start)
        assertEquals(restored.length, link.end)
        assertEquals(expectedUrl, (link.item as LinkAnnotation.Url).url)
    }
}
