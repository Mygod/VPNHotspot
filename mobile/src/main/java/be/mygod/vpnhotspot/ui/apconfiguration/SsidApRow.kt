package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.autofill.contentType
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.ui.PreferenceRow
import be.mygod.vpnhotspot.ui.TooltipIconButton
import be.mygod.vpnhotspot.ui.annotatedStringResource
import be.mygod.vpnhotspot.ui.rememberTextFieldValueAtEnd
import be.mygod.vpnhotspot.util.readableMessage

@Composable
internal fun SsidApRow(state: ApConfigurationState) {
    val context = LocalContext.current
    var editing by rememberSaveable(state.ssid) { mutableStateOf(false) }
    var draft by rememberTextFieldValueAtEnd(state.ssid, editing)
    var draftHex by rememberSaveable(state.ssid, editing) { mutableStateOf(state.ssidHex) }
    var error by remember(editing) { mutableStateOf<String?>(null) }
    val draftError = error ?: state.ssidError(draft.text, draftHex, context)
    val draftByteCount = state.ssidByteCount(draft.text, draftHex)
    PreferenceRow(
        icon = R.drawable.ic_device_network_wifi,
        title = stringResource(R.string.wifi_ssid),
        summaryContent = {
            Column {
                Text(state.ssid)
                state.ssidSafeModeWarning(state.ssid, state.ssidHex, context)?.let { ErrorApText(it) }
            }
        },
        enabled = true,
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
                        .contentType(WIFI_SSID_CONTENT_TYPE),
                    singleLine = true,
                    isError = draftError != null,
                    supportingText = {
                        Column {
                            draftError?.let { ErrorApText(it) } ?: state.ssidSafeModeWarning(
                                draft.text,
                                draftHex,
                                context,
                            )?.let {
                                ErrorApText(it)
                            }
                            Text("$draftByteCount/32")
                        }
                    },
                    trailingIcon = if (state.canToggleSsidHex) {
                        {
                            val tooltip = stringResource(R.string.wifi_ssid_toggle_hex)
                            TooltipIconButton(
                                tooltip = tooltip,
                                onClick = {
                                    try {
                                        val converted = state.convertSsidDisplay(draft.text, draftHex)
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
                                        R.drawable.ic_av_closed_caption
                                    } else R.drawable.ic_av_closed_caption_off),
                                    contentDescription = tooltip,
                                )
                            }
                        }
                    } else null,
                )
            }
        },
        confirmButton = {
            TextButton(
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
            TextButton(onClick = { editing = false }) {
                Text(stringResource(android.R.string.cancel))
            }
        },
    )
}
