package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.width
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.VerticalDivider
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.autofill.ContentType
import androidx.compose.ui.autofill.contentType
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.ui.DialogConfirmButton
import be.mygod.vpnhotspot.ui.DialogDismissButton
import be.mygod.vpnhotspot.ui.PreferenceRow
import be.mygod.vpnhotspot.ui.PreferenceSplitControlWidth
import be.mygod.vpnhotspot.ui.TooltipIconButton
import be.mygod.vpnhotspot.ui.annotatedStringResource
import be.mygod.vpnhotspot.ui.rememberDialogFocusRequester
import be.mygod.vpnhotspot.ui.rememberTextFieldValueAtEnd
import be.mygod.vpnhotspot.util.readableMessage

@Composable
fun SsidApRow(
    state: ApConfigurationState,
    onShowQrCode: () -> Unit,
) {
    val context = LocalContext.current
    var editing by rememberSaveable(state.ssid) { mutableStateOf(false) }
    var draft by rememberTextFieldValueAtEnd(state.ssid, editing)
    var draftHex by rememberSaveable(state.ssid, editing) { mutableStateOf(state.ssidHex) }
    var error by remember(editing) { mutableStateOf<String?>(null) }
    val draftError = error ?: state.ssidError(draft.text, draftHex, context)
    val draftByteCount = state.ssidByteCount(draft.text, draftHex)
    PreferenceRow(
        icon = R.drawable.ic_network_wifi,
        title = stringResource(R.string.wifi_ssid),
        summaryContent = {
            Column {
                Text(state.ssid)
                state.ssidSafeModeWarning(state.ssid, state.ssidHex, context)?.let { ErrorApText(it) }
            }
        },
        trailing = {
            val tooltip = stringResource(R.string.configuration_share)
            Row(verticalAlignment = Alignment.CenterVertically) {
                VerticalDivider(
                    modifier = Modifier.height(40.dp),
                    color = MaterialTheme.colorScheme.outlineVariant,
                )
                Spacer(Modifier.width(12.dp))
                Box(
                    modifier = Modifier
                        .width(PreferenceSplitControlWidth)
                        .height(48.dp),
                    contentAlignment = Alignment.Center,
                ) {
                    TooltipIconButton(
                        tooltip = tooltip,
                        enabled = state.canShare,
                        onClick = onShowQrCode,
                    ) {
                        Icon(
                            painter = painterResource(R.drawable.ic_qr_code_2),
                            contentDescription = tooltip,
                        )
                    }
                }
            }
        },
        onClick = { editing = true },
    )
    if (editing) AlertDialog(
        onDismissRequest = { editing = false },
        title = { Text(stringResource(R.string.wifi_ssid)) },
        text = {
            val focusRequester = rememberDialogFocusRequester()
            Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
                Text(
                    text = annotatedStringResource(R.string.wifi_ssid_help),
                    style = MaterialTheme.typography.bodyMedium,
                )
                OutlinedTextField(
                    value = draft,
                    onValueChange = {
                        draft = it
                        error = null
                    },
                    modifier = Modifier
                        .fillMaxWidth()
                        .focusRequester(focusRequester)
                        .contentType(ContentType.NewUsername + ContentType.Username),
                    singleLine = true,
                    isError = draftError != null,
                    shape = OutlinedTextFieldDefaults.roundedShape,
                    colors = OutlinedTextFieldDefaults.tonalColors(),
                    supportingText = {
                        Column {
                            draftError?.let { ErrorApText(it) } ?: state.ssidSafeModeWarning(
                                draft.text,
                                draftHex,
                                context,
                            )?.let {
                                ErrorApText(it)
                            }
                            Text(stringResource(R.string.configuration_input_length, draftByteCount, 32))
                        }
                    },
                    trailingIcon = if (state.canToggleSsidHex) {
                        {
                            val tooltip = stringResource(R.string.wifi_ssid_toggle_hex)
                            TooltipIconButton(
                                tooltip = tooltip,
                                onClick = {
                                    try {
                                        val converted = state.convertSsidDisplay(draft.text, draftHex, context)
                                        draft = TextFieldValue(converted, TextRange(converted.length))
                                        draftHex = !draftHex
                                        error = null
                                    } catch (e: RuntimeException) {
                                        error = e.readableMessage
                                    }
                                },
                            ) {
                                Icon(
                                    painter = painterResource(if (draftHex) {
                                        R.drawable.ic_abc
                                    } else R.drawable.ic_numbers),
                                    contentDescription = tooltip,
                                )
                            }
                        }
                    } else null,
                )
            }
        },
        confirmButton = {
            DialogConfirmButton(
                enabled = draftError == null,
                onClick = {
                    state.setSsid(draft.text, draftHex)
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
