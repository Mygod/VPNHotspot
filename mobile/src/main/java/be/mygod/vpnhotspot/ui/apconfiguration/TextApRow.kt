package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.ui.DialogConfirmButton
import be.mygod.vpnhotspot.ui.DialogDismissButton
import be.mygod.vpnhotspot.ui.PreferenceRow
import be.mygod.vpnhotspot.ui.rememberTextFieldValueAtEnd

@Composable
fun TextApRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    value: String,
    readOnly: Boolean,
    summary: String = value,
    description: AnnotatedString? = null,
    keyboardType: KeyboardType = KeyboardType.Text,
    keyboardOptions: KeyboardOptions = KeyboardOptions(keyboardType = keyboardType),
    maxLength: Int? = null,
    minLines: Int = if (value.contains('\n')) 3 else 1,
    placeholder: String? = null,
    suffix: String? = null,
    supportingText: String? = null,
    validator: (String) -> String? = { null },
    onValueChange: (String) -> Unit,
) {
    var editing by rememberSaveable(value) { mutableStateOf(false) }
    var draft by rememberTextFieldValueAtEnd(value, editing)
    val error = validator(draft.text)
    PreferenceRow(
        icon = icon,
        title = stringResource(title),
        summary = summary,
        enabled = !readOnly,
        onClick = { editing = true },
    )
    if (editing) AlertDialog(
        onDismissRequest = { editing = false },
        title = { Text(stringResource(title)) },
        text = {
            val focusRequester = rememberDialogFocusRequester()
            Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
                description?.let {
                    Text(
                        text = it,
                        style = MaterialTheme.typography.bodyMedium,
                    )
                }
                OutlinedTextField(
                    value = draft,
                    onValueChange = { draft = maxLength?.let(it::takeText) ?: it },
                    modifier = Modifier
                        .fillMaxWidth()
                        .focusRequester(focusRequester),
                    keyboardOptions = keyboardOptions,
                    placeholder = placeholder?.let { { Text(it) } },
                    singleLine = minLines == 1,
                    minLines = minLines,
                    isError = error != null,
                    suffix = suffix?.let { { Text(it) } },
                    supportingText = if (error != null || supportingText != null || maxLength != null) {
                        {
                            Column {
                                error?.let { ErrorApText(it) }
                                if (error == null) supportingText?.let { Text(it) }
                                maxLength?.let { Text("${draft.text.length}/$it") }
                            }
                        }
                    } else null,
                )
            }
        },
        confirmButton = {
            DialogConfirmButton(
                enabled = error == null,
                onClick = {
                    onValueChange(draft.text)
                    editing = false
                },
            ) {
                Text(stringResource(android.R.string.ok))
            }
        },
        dismissButton = {
            DialogDismissButton(onClick = { editing = false }) {
                Text(stringResource(android.R.string.cancel))
            }
        },
    )
}
