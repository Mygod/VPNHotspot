package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.error
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue

fun TextFieldValue.takeText(maxLength: Int): TextFieldValue {
    if (text.length <= maxLength) return this
    val text = text.take(maxLength)
    return TextFieldValue(
        text,
        TextRange(selection.start.coerceIn(0, text.length), selection.end.coerceIn(0, text.length)),
    )
}

@Composable
fun ErrorApText(text: String, modifier: Modifier = Modifier) {
    Text(
        text = text,
        color = MaterialTheme.colorScheme.error,
        modifier = modifier.semantics {
            liveRegion = LiveRegionMode.Polite
            error(text)
        },
    )
}
