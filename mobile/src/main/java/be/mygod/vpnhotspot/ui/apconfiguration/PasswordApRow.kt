package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.text.input.InputTransformation
import androidx.compose.foundation.text.input.TextFieldState
import androidx.compose.foundation.text.input.TextObfuscationMode
import androidx.compose.foundation.text.input.maxLength
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedSecureTextField
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
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.ui.PreferenceRow
import be.mygod.vpnhotspot.ui.TooltipIconButton
import be.mygod.vpnhotspot.ui.annotatedStringResource

@Composable
internal fun PasswordApRow(state: ApConfigurationState) {
    if (!state.passwordEnabled) return
    val context = LocalContext.current
    val maxLength = state.passwordMaxLength
    var editing by rememberSaveable(state.password) { mutableStateOf(false) }
    val draft = rememberSaveable(state.password, editing, saver = TextFieldState.Saver) {
        TextFieldState(state.password)
    }
    var visible by rememberSaveable(editing) { mutableStateOf(false) }
    val password = draft.text.toString()
    val error = state.passwordError(password, context)
    PreferenceRow(
        icon = R.drawable.ic_device_wifi_lock,
        title = stringResource(R.string.wifi_password),
        summary = if (state.password.isEmpty()) "" else "\u2022".repeat(8),
        onClick = { editing = true },
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
                OutlinedSecureTextField(
                    state = draft,
                    modifier = Modifier
                        .fillMaxWidth()
                        .focusRequester(focusRequester)
                        .contentType(WIFI_PASSWORD_CONTENT_TYPE),
                    inputTransformation = if (maxLength) InputTransformation.maxLength(63) else null,
                    isError = error != null,
                    supportingText = if (error != null || maxLength) {
                        {
                            Column {
                                error?.let { ErrorApText(it) }
                                if (maxLength) Text("${password.length}/63")
                            }
                        }
                    } else null,
                    trailingIcon = {
                        val tooltip = stringResource(
                            if (visible) R.string.wifi_password_hide else R.string.wifi_password_show,
                        )
                        TooltipIconButton(
                            tooltip = tooltip,
                            onClick = { visible = !visible },
                        ) {
                            Icon(
                                painter = painterResource(if (visible) {
                                    R.drawable.ic_action_visibility_off
                                } else R.drawable.ic_image_remove_red_eye),
                                contentDescription = tooltip,
                            )
                        }
                    },
                    textStyle = LocalTextStyle.current.copy(fontFamily = FontFamily.Monospace),
                    textObfuscationMode = if (visible) {
                        TextObfuscationMode.Visible
                    } else TextObfuscationMode.Hidden,
                )
            }
        },
        confirmButton = {
            TextButton(
                enabled = error == null,
                onClick = {
                    state.password = password
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
