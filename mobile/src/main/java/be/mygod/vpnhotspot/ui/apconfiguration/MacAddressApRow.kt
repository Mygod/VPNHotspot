package be.mygod.vpnhotspot.ui.apconfiguration

import android.os.Build
import androidx.compose.foundation.gestures.Orientation
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.scrollbar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.ui.DialogConfirmButton
import be.mygod.vpnhotspot.ui.DialogDismissButton
import be.mygod.vpnhotspot.ui.PreferenceGroup
import be.mygod.vpnhotspot.ui.PreferenceRadioRow
import be.mygod.vpnhotspot.ui.PreferenceRow
import be.mygod.vpnhotspot.ui.annotatedStringResource
import be.mygod.vpnhotspot.ui.rememberDialogFocusRequester
import be.mygod.vpnhotspot.ui.rememberTextFieldValueAtEnd

@Composable
fun MacAddressApRow(state: ApConfigurationState) {
    val context = LocalContext.current
    var editing by rememberSaveable { mutableStateOf(false) }
    val enabled = !state.readOnly
    PreferenceRow(
        icon = R.drawable.ic_fingerprint,
        title = stringResource(R.string.wifi_advanced_mac_address_title),
        summary = state.macAddressSummary(context),
        enabled = enabled,
        onClick = if (enabled) ({ editing = true }) else null,
    )
    if (editing) MacAddressApDialog(
        state = state,
        onDismissRequest = { editing = false },
    )
}

@Composable
private fun MacAddressApDialog(
    state: ApConfigurationState,
    onDismissRequest: () -> Unit,
) {
    var draftRandomization by rememberSaveable { mutableIntStateOf(state.macRandomization) }
    var draftBssid by rememberTextFieldValueAtEnd(state.bssid)
    var draftPersistentRandomizedMac by rememberTextFieldValueAtEnd(state.persistentRandomizedMac)
    val randomizationVisible = !state.p2pMode && Build.VERSION.SDK_INT >= 31
    val bssidEnabled = state.bssidEditable(draftRandomization)
    val persistentRandomizedMacVisible = !state.p2pMode && Build.VERSION.SDK_INT >= 33
    val bssidError = if (bssidEnabled) {
        validateOptionalMac(draftBssid.text) { mac ->
            if (Build.VERSION.SDK_INT >= 30 && !state.p2pMode) {
                SoftApConfigurationCompat.testPlatformValidity(mac)
            }
        }
    } else null
    val persistentRandomizedMacError = if (persistentRandomizedMacVisible) {
        validateOptionalMac(draftPersistentRandomizedMac.text)
    } else null
    AlertDialog(
        onDismissRequest = onDismissRequest,
        title = { Text(stringResource(R.string.wifi_advanced_mac_address_title)) },
        text = {
            val bssidFocusRequester = rememberDialogFocusRequester(
                bssidEnabled && (state.p2pMode ||
                        draftRandomization == SoftApConfigurationCompat.RANDOMIZATION_NONE),
            )
            val persistentFocusRequester = rememberDialogFocusRequester(
                persistentRandomizedMacVisible &&
                        draftRandomization == SoftApConfigurationCompat.RANDOMIZATION_PERSISTENT,
            )
            val scrollState = rememberScrollState()
            val bssidField: @Composable () -> Unit = {
                OutlinedTextField(
                    value = draftBssid,
                    onValueChange = { draftBssid = it.takeText(17) },
                    modifier = Modifier
                        .fillMaxWidth()
                        .focusRequester(bssidFocusRequester),
                    enabled = bssidEnabled,
                    label = { Text(stringResource(R.string.wifi_advanced_mac_address_title)) },
                    keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                    singleLine = true,
                    isError = bssidError != null,
                    shape = OutlinedTextFieldDefaults.roundedShape,
                    supportingText = bssidError?.let { { ErrorApText(it) } },
                )
            }
            Column(
                modifier = Modifier
                    .scrollbar(
                        state = scrollState.scrollIndicatorState,
                        orientation = Orientation.Vertical,
                        isFadeEnabled = false,
                    )
                    .verticalScroll(scrollState),
                verticalArrangement = Arrangement.spacedBy(16.dp),
            ) {
                Text(
                    text = annotatedStringResource(if (state.p2pMode) {
                        R.string.wifi_p2p_mac_address_help
                    } else R.string.wifi_mac_address_help),
                    style = MaterialTheme.typography.bodyMedium,
                )
                if (state.p2pMode) {
                    PreferenceGroup(horizontalPadding = 0.dp) {
                        row(SoftApConfigurationCompat.RANDOMIZATION_NONE) {
                            MacRandomizationOptionRow(
                                selected = draftRandomization == SoftApConfigurationCompat.RANDOMIZATION_NONE,
                                title = stringResource(R.string.wifi_mac_randomization_none),
                                description = annotatedStringResource(R.string.wifi_mac_randomization_none_help),
                                onSelect = { draftRandomization = SoftApConfigurationCompat.RANDOMIZATION_NONE },
                            )
                        }
                        row(SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT) {
                            MacRandomizationOptionRow(
                                selected = draftRandomization ==
                                        SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT,
                                title = stringResource(R.string.wifi_mac_randomization_non_persistent),
                                description = annotatedStringResource(
                                    R.string.wifi_mac_randomization_non_persistent_help),
                                onSelect = {
                                    draftRandomization = SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT
                                },
                            )
                        }
                    }
                } else {
                    PreferenceGroup(horizontalPadding = 0.dp) {
                        row(SoftApConfigurationCompat.RANDOMIZATION_NONE) {
                            MacRandomizationOptionRow(
                                selected = draftRandomization == SoftApConfigurationCompat.RANDOMIZATION_NONE,
                                showRadio = randomizationVisible,
                                title = stringResource(R.string.wifi_mac_randomization_none),
                                description = annotatedStringResource(R.string.wifi_mac_randomization_none_help),
                                onSelect = { draftRandomization = SoftApConfigurationCompat.RANDOMIZATION_NONE },
                            ) {
                                bssidField()
                            }
                        }
                        if (randomizationVisible) {
                            row(SoftApConfigurationCompat.RANDOMIZATION_PERSISTENT) {
                                MacRandomizationOptionRow(
                                    selected = draftRandomization == SoftApConfigurationCompat.RANDOMIZATION_PERSISTENT,
                                    title = stringResource(R.string.wifi_mac_randomization_persistent),
                                    description = annotatedStringResource(
                                        R.string.wifi_mac_randomization_persistent_help),
                                    onSelect = {
                                        draftRandomization = SoftApConfigurationCompat.RANDOMIZATION_PERSISTENT
                                    },
                                ) {
                                    if (persistentRandomizedMacVisible) {
                                        OutlinedTextField(
                                            value = draftPersistentRandomizedMac,
                                            onValueChange = { draftPersistentRandomizedMac = it.takeText(17) },
                                            modifier = Modifier
                                                .fillMaxWidth()
                                                .focusRequester(persistentFocusRequester),
                                            label = {
                                                Text(stringResource(
                                                    R.string.wifi_advanced_mac_address_persistent_randomized))
                                            },
                                            keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                                            singleLine = true,
                                            isError = persistentRandomizedMacError != null,
                                            shape = OutlinedTextFieldDefaults.roundedShape,
                                            supportingText = persistentRandomizedMacError?.let { { ErrorApText(it) } },
                                        )
                                    }
                                }
                            }
                            row(SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT) {
                                MacRandomizationOptionRow(
                                    selected = draftRandomization ==
                                            SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT,
                                    title = stringResource(R.string.wifi_mac_randomization_non_persistent),
                                    description = annotatedStringResource(
                                        R.string.wifi_mac_randomization_non_persistent_help),
                                    onSelect = {
                                        draftRandomization = SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT
                                    },
                                )
                            }
                        }
                    }
                }
            }
        },
        confirmButton = {
            DialogConfirmButton(
                enabled = bssidError == null && persistentRandomizedMacError == null,
                onClick = {
                    state.macRandomization = draftRandomization
                    state.bssid = if (state.p2pMode) "" else draftBssid.text
                    if (persistentRandomizedMacVisible) {
                        state.persistentRandomizedMac = draftPersistentRandomizedMac.text
                    }
                    onDismissRequest()
                },
            ) {
                Text(stringResource(android.R.string.ok))
            }
        },
        dismissButton = {
            DialogDismissButton(onClick = onDismissRequest) {
                Text(stringResource(android.R.string.cancel))
            }
        },
    )
}

@Composable
private fun MacRandomizationOptionRow(
    selected: Boolean,
    showRadio: Boolean = true,
    title: String,
    description: AnnotatedString,
    onSelect: () -> Unit,
    content: @Composable (() -> Unit)? = null,
) {
    val summaryContent: @Composable () -> Unit = {
        Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            Text(description)
            content?.invoke()
        }
    }
    if (showRadio) {
        PreferenceRadioRow(
            selected = selected,
            titleContent = { Text(title) },
            summaryContent = summaryContent,
            onClick = onSelect,
        )
    } else {
        PreferenceRow(
            titleContent = { Text(title) },
            summaryContent = summaryContent,
        )
    }
}
