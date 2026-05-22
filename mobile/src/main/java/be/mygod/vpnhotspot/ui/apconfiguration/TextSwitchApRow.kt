package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
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
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.ui.DialogConfirmButton
import be.mygod.vpnhotspot.ui.DialogDismissButton
import be.mygod.vpnhotspot.ui.PreferenceRow
import be.mygod.vpnhotspot.ui.PreferenceSplitSwitch
import be.mygod.vpnhotspot.ui.PreferenceSwitch
import be.mygod.vpnhotspot.ui.rememberTextFieldValueAtEnd

@Composable
fun TextSwitchApRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    @StringRes valueTitle: Int,
    checked: Boolean,
    value: String,
    valueSummary: String,
    switchReadOnly: Boolean,
    valueReadOnly: Boolean,
    summary: AnnotatedString? = null,
    description: AnnotatedString? = null,
    keyboardType: KeyboardType = KeyboardType.Text,
    keyboardOptions: KeyboardOptions = KeyboardOptions(keyboardType = keyboardType),
    maxLength: Int? = null,
    minLines: Int = if (value.contains('\n')) 3 else 1,
    placeholder: String? = null,
    suffix: String? = null,
    supportingText: String? = null,
    validator: (String) -> String? = { null },
    onCheckedChange: (Boolean) -> Unit,
    onValueChange: (String) -> Unit,
) {
    var editing by rememberSaveable(value) { mutableStateOf(false) }
    var draft by rememberTextFieldValueAtEnd(value, editing)
    val error = validator(draft.text)
    PreferenceRow(
        icon = icon,
        title = stringResource(title),
        summaryContent = if (summary == null && valueSummary.isEmpty()) null else {
            {
                Column {
                    summary?.let { Text(it) }
                    if (valueSummary.isNotEmpty()) Text(valueSummary)
                }
            }
        },
        enabled = true,
        trailing = {
            PreferenceSplitSwitch(
                checked = checked,
                enabled = !switchReadOnly,
                onCheckedChange = onCheckedChange,
            )
        },
        onClick = { editing = true },
    )
    if (editing) {
        val fieldEnabled = !valueReadOnly
        AlertDialog(
            onDismissRequest = { editing = false },
            title = {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        text = stringResource(title),
                        modifier = Modifier.weight(1f),
                    )
                    PreferenceSwitch(
                        checked = checked,
                        enabled = !switchReadOnly,
                        onCheckedChange = if (switchReadOnly) null else onCheckedChange,
                    )
                }
            },
            text = {
                val focusRequester = rememberDialogFocusRequester(fieldEnabled)
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
                        enabled = fieldEnabled,
                        label = { Text(stringResource(valueTitle)) },
                        keyboardOptions = keyboardOptions,
                        placeholder = placeholder?.let { { Text(it) } },
                        singleLine = minLines == 1,
                        minLines = minLines,
                        isError = fieldEnabled && error != null,
                        suffix = suffix?.let { { Text(it) } },
                        supportingText = if ((fieldEnabled && error != null) || supportingText != null ||
                            maxLength != null) {
                            {
                                Column {
                                    if (fieldEnabled) error?.let { ErrorApText(it) }
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
                    enabled = !fieldEnabled || error == null,
                    onClick = {
                        if (fieldEnabled) onValueChange(draft.text)
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
}
