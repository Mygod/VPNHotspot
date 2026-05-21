package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue

@Composable
internal fun rememberDialogFocusRequester(enabled: Boolean = true): FocusRequester {
    val focusRequester = remember { FocusRequester() }
    val keyboard = LocalSoftwareKeyboardController.current
    LaunchedEffect(enabled) {
        if (enabled) {
            focusRequester.requestFocus()
            keyboard?.show()
        }
    }
    return focusRequester
}

internal fun TextFieldValue.takeText(maxLength: Int): TextFieldValue {
    if (text.length <= maxLength) return this
    val text = text.take(maxLength)
    return TextFieldValue(
        text,
        TextRange(selection.start.coerceIn(0, text.length), selection.end.coerceIn(0, text.length)),
    )
}

@Composable
internal fun ErrorApText(text: String, modifier: Modifier = Modifier) {
    Text(
        text = text,
        color = MaterialTheme.colorScheme.error,
        modifier = modifier,
    )
}
