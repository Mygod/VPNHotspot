package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.FloatingActionButtonDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.contentColorFor
import androidx.compose.runtime.Composable
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.disabled
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.ui.TooltipIconButton
import be.mygod.vpnhotspot.ui.showLongSnackbar
import be.mygod.vpnhotspot.util.readableMessage
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch
import timber.log.Timber

@Composable
fun ApConfigurationTopBarActions(
    state: ApConfigurationState,
    snackbarHostState: SnackbarHostState,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    if (state.possiblyInvalid(context)) Icon(
        painter = painterResource(R.drawable.ic_alert_warning),
        contentDescription = stringResource(R.string.configuration_invalid),
        tint = MaterialTheme.colorScheme.error,
        modifier = Modifier.size(24.dp),
    )
    TooltipIconButton(
        tooltip = stringResource(android.R.string.copy),
        enabled = state.copyError(context) == null,
        onClick = {
            try {
                state.copyToClipboard()
            } catch (e: RuntimeException) {
                scope.launch { snackbarHostState.showLongSnackbar(e.readableMessage) }
            }
        },
    ) {
        Icon(
            painter = painterResource(R.drawable.ic_content_file_copy),
            contentDescription = stringResource(android.R.string.copy),
        )
    }
    TooltipIconButton(
        tooltip = stringResource(android.R.string.paste),
        onClick = {
            try {
                state.pasteFromClipboard()
            } catch (e: RuntimeException) {
                scope.launch { snackbarHostState.showLongSnackbar(e.readableMessage) }
            }
        },
    ) {
        Icon(
            painter = painterResource(R.drawable.ic_content_content_paste),
            contentDescription = stringResource(android.R.string.paste),
        )
    }
}

@Composable
fun ApConfigurationSaveFab(
    state: ApConfigurationState,
    onApply: suspend (SoftApConfigurationCompat) -> Boolean,
    snackbarHostState: SnackbarHostState,
    onApplied: () -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val save = stringResource(R.string.wifi_save)
    val canSave = state.canSave(context)
    val containerColor = if (canSave) {
        FloatingActionButtonDefaults.containerColor
    } else {
        MaterialTheme.colorScheme.onSurface.copy(alpha = 0.12f)
    }
    val contentColor = if (canSave) {
        contentColorFor(containerColor)
    } else {
        MaterialTheme.colorScheme.onSurface.copy(alpha = 0.38f)
    }
    ExtendedFloatingActionButton(
        text = { Text(save) },
        icon = {
            Icon(
                painter = painterResource(R.drawable.ic_content_save),
                contentDescription = null,
            )
        },
        onClick = {
            if (canSave) {
                val config = state.generateConfig()
                scope.launch {
                    try {
                        if (onApply(config)) onApplied()
                    } catch (e: CancellationException) {
                        throw e
                    } catch (e: Exception) {
                        Timber.w(e)
                        snackbarHostState.showLongSnackbar(e.readableMessage)
                    }
                }
            }
        },
        modifier = (if (canSave) Modifier else Modifier.semantics { disabled() }).navigationBarsPadding(),
        containerColor = containerColor,
        contentColor = contentColor,
    )
}
