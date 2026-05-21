package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.autofill.contentType
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.ui.PreferenceRow
import be.mygod.vpnhotspot.ui.TooltipIconButton
import be.mygod.vpnhotspot.ui.annotatedStringResource
import be.mygod.vpnhotspot.ui.rememberTextFieldValueAtEnd

@Composable
internal fun PasswordApRow(state: ApConfigurationState) {
    val context = LocalContext.current
    val enabled = state.passwordEnabled
    val maxLength = state.passwordMaxLength
    var editing by rememberSaveable(state.password) { mutableStateOf(false) }
    var draft by rememberTextFieldValueAtEnd(state.password, editing)
    var visible by rememberSaveable(editing) { mutableStateOf(false) }
    val error = state.passwordError(draft.text, context)
    PreferenceRow(
        icon = R.drawable.ic_device_wifi_lock,
        title = stringResource(R.string.wifi_password),
        summary = if (!enabled || state.password.isEmpty()) "" else "\u2022".repeat(8),
        enabled = enabled,
        onClick = if (enabled) ({ editing = true }) else null,
    )
    if (editing) AlertDialog(
        onDismissRequest = { editing = false },
        title = { Text(stringResource(R.string.wifi_password)) },
        text = {
            val focusRequester = rememberDialogFocusRequester()
            Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
                Text(
                    text = annotatedStringResource(R.string.wifi_password_help),
                    style = MaterialTheme.typography.bodyMedium,
                )
                OutlinedTextField(
                    value = draft,
                    onValueChange = { draft = if (maxLength) it.takeText(63) else it },
                    modifier = Modifier
                        .fillMaxWidth()
                        .focusRequester(focusRequester)
                        .contentType(WIFI_PASSWORD_CONTENT_TYPE),
                    keyboardOptions = KeyboardOptions(
                        autoCorrectEnabled = false,
                        keyboardType = KeyboardType.Password,
                    ),
                    singleLine = true,
                    isError = error != null,
                    supportingText = if (error != null || maxLength) {
                        {
                            Column {
                                error?.let { ErrorApText(it) }
                                if (maxLength) Text("${draft.text.length}/63")
                            }
                        }
                    } else null,
                    trailingIcon = {
                        TooltipIconButton(
                            tooltip = stringResource(R.string.wifi_password),
                            onClick = { visible = !visible },
                        ) {
                            Icon(
                                painter = painterResource(R.drawable.ic_image_remove_red_eye),
                                contentDescription = stringResource(R.string.wifi_password),
                            )
                        }
                    },
                    textStyle = LocalTextStyle.current.copy(fontFamily = FontFamily.Monospace),
                    visualTransformation = if (visible) VisualTransformation.None else PasswordVisualTransformation(),
                )
            }
        },
        confirmButton = {
            TextButton(
                enabled = error == null,
                onClick = {
                    state.password = draft.text
                    editing = false
                },
            ) {
                Text(stringResource(android.R.string.ok))
            }
        },
        dismissButton = {
            TextButton(onClick = { editing = false }) {
                Text(stringResource(android.R.string.cancel))
            }
        },
    )
}
