package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.compose.foundation.layout.size
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.PlainTooltip
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TooltipAnchorPosition
import androidx.compose.material3.TooltipBox
import androidx.compose.material3.TooltipDefaults
import androidx.compose.material3.rememberTooltipState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.ui.TooltipIconButton
import be.mygod.vpnhotspot.ui.showLongSnackbar
import be.mygod.vpnhotspot.util.readableMessage
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.DelicateCoroutinesApi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber

@Composable
@OptIn(DelicateCoroutinesApi::class)
internal fun ApConfigurationTopBarActions(
    state: ApConfigurationState,
    session: ApConfigurationSession,
    snackbarHostState: SnackbarHostState,
    onApplied: () -> Unit,
) {
    val context = LocalContext.current
    rememberCoroutineScope().let { scope ->
        if (state.possiblyInvalid(context)) Icon(
            painter = painterResource(R.drawable.ic_alert_warning),
            contentDescription = stringResource(R.string.configuration_invalid),
            tint = MaterialTheme.colorScheme.error,
            modifier = Modifier.size(24.dp),
        )
        TooltipIconButton(
            tooltip = stringResource(android.R.string.copy),
            enabled = state.canCopy(context),
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
                painter = painterResource(R.drawable.ic_content_paste),
                contentDescription = stringResource(android.R.string.paste),
            )
        }
        if (!state.readOnly) {
            val save = stringResource(R.string.wifi_save)
            TooltipBox(
                positionProvider = TooltipDefaults.rememberTooltipPositionProvider(TooltipAnchorPosition.Above),
                tooltip = { PlainTooltip { Text(save) } },
                state = rememberTooltipState(),
            ) {
                FilledTonalIconButton(
                    enabled = state.canSave(context),
                    onClick = {
                        val config = state.generateConfig()
                        GlobalScope.launch(Dispatchers.Main.immediate) {
                            try {
                                if (session.onApply(config)) onApplied()
                            } catch (e: CancellationException) {
                                throw e
                            } catch (e: Exception) {
                                Timber.w(e)
                                snackbarHostState.showLongSnackbar(e.readableMessage)
                            }
                        }
                    },
                ) {
                    Icon(
                        painter = painterResource(R.drawable.ic_content_save),
                        contentDescription = save,
                    )
                }
            }
        }
    }
}
