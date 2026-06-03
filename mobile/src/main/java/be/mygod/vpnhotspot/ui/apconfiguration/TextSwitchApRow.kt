package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.foundation.gestures.Orientation
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.input.InputTransformation
import androidx.compose.foundation.text.input.TextFieldLineLimits
import androidx.compose.foundation.text.input.TextFieldState
import androidx.compose.foundation.text.input.maxLength
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextFieldLabelPosition
import androidx.compose.material3.scrollbar
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
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.ui.DialogConfirmButton
import be.mygod.vpnhotspot.ui.DialogDismissButton
import be.mygod.vpnhotspot.ui.PreferenceRow
import be.mygod.vpnhotspot.ui.PreferenceSplitSwitch
import be.mygod.vpnhotspot.ui.PreferenceSwitch
import be.mygod.vpnhotspot.ui.rememberDialogFocusRequester

@Composable
fun TextSwitchApRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    @StringRes valueTitle: Int,
    checked: Boolean,
    value: String,
    valueSummary: String = "",
    valueReadOnly: Boolean = false,
    summary: AnnotatedString? = null,
    description: AnnotatedString,
    keyboardType: KeyboardType = KeyboardType.Text,
    keyboardOptions: KeyboardOptions = KeyboardOptions(keyboardType = keyboardType),
    maxLength: Int? = null,
    minLines: Int = if (value.contains('\n')) 3 else 1,
    placeholder: String? = null,
    suffix: String? = null,
    validator: (String) -> String?,
    onCheckedChange: (Boolean) -> Unit,
    onValueChange: (String) -> Unit,
) {
    var editing by rememberSaveable(value) { mutableStateOf(false) }
    val draft = rememberSaveable(value, editing, saver = TextFieldState.Saver) {
        TextFieldState(value)
    }
    val text = draft.text.toString()
    val error = validator(text)
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
        trailing = {
            PreferenceSplitSwitch(
                checked = checked,
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
                        onCheckedChange = onCheckedChange,
                    )
                }
            },
            text = {
                val focusRequester = rememberDialogFocusRequester(fieldEnabled)
                val multiline = minLines > 1
                val scrollState = rememberScrollState()
                Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
                    Text(
                        text = description,
                        style = MaterialTheme.typography.bodyMedium,
                    )
                    OutlinedTextField(
                        state = draft,
                        modifier = Modifier
                            .fillMaxWidth()
                            .focusRequester(focusRequester)
                            .then(
                                if (multiline) {
                                    Modifier.scrollbar(
                                        state = scrollState.scrollIndicatorState,
                                        orientation = Orientation.Vertical,
                                        isFadeEnabled = false,
                                    )
                                } else Modifier
                            ),
                        enabled = fieldEnabled,
                        inputTransformation = maxLength?.let { InputTransformation.maxLength(it) },
                        labelPosition = TextFieldLabelPosition.Inside(),
                        label = { Text(stringResource(valueTitle)) },
                        keyboardOptions = keyboardOptions,
                        placeholder = placeholder?.let { { Text(it) } },
                        lineLimits = if (multiline) TextFieldLineLimits.MultiLine(
                            minHeightInLines = minLines,
                            maxHeightInLines = minLines,
                        ) else TextFieldLineLimits.SingleLine,
                        scrollState = scrollState,
                        isError = fieldEnabled && error != null,
                        shape = OutlinedTextFieldDefaults.roundedShape,
                        colors = OutlinedTextFieldDefaults.tonalColors(),
                        suffix = suffix?.let { { Text(it) } },
                        supportingText = if ((fieldEnabled && error != null) || maxLength != null) {
                            {
                                Column {
                                    if (fieldEnabled) error?.let { ErrorApText(it) }
                                    maxLength?.let { Text(stringResource(R.string.configuration_input_length, text.length, it)) }
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
                        if (fieldEnabled) onValueChange(text)
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
