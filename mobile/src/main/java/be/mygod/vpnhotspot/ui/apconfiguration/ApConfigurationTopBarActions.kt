package be.mygod.vpnhotspot.ui.apconfiguration

import android.graphics.Bitmap
import android.graphics.Color
import android.widget.Toast
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.size
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.dimensionResource
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.core.graphics.set
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.ui.TooltipIconButton
import be.mygod.vpnhotspot.ui.showLongSnackbar
import be.mygod.vpnhotspot.util.readableMessage
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.MultiFormatWriter
import com.google.zxing.WriterException
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.DelicateCoroutinesApi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber
import java.nio.charset.StandardCharsets

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
        var qrCode by rememberSaveable { mutableStateOf<String?>(null) }
        var overflowExpanded by remember { mutableStateOf(false) }
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
        val overflowDescription = stringResource(R.string.action_menu_overflow_description)
        TooltipIconButton(
            tooltip = overflowDescription,
            onClick = { overflowExpanded = true },
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_more_vert),
                contentDescription = overflowDescription,
            )
        }
        DropdownMenu(expanded = overflowExpanded, onDismissRequest = { overflowExpanded = false }) {
            DropdownMenuItem(
                text = { Text(stringResource(android.R.string.paste)) },
                leadingIcon = {
                    Icon(
                        painter = painterResource(R.drawable.ic_content_paste),
                        contentDescription = null,
                    )
                },
                onClick = {
                    overflowExpanded = false
                    try {
                        state.pasteFromClipboard()
                    } catch (e: RuntimeException) {
                        scope.launch { snackbarHostState.showLongSnackbar(e.readableMessage) }
                    }
                },
            )
            DropdownMenuItem(
                text = { Text(stringResource(R.string.configuration_share)) },
                enabled = state.canShare,
                leadingIcon = {
                    Icon(
                        painter = painterResource(R.drawable.ic_settings_qrcode),
                        contentDescription = null,
                    )
                },
                onClick = {
                    overflowExpanded = false
                    try {
                        qrCode = state.generateConfig(requirePassword = false, full = false).toQrCode()
                    } catch (e: RuntimeException) {
                        scope.launch { snackbarHostState.showLongSnackbar(e.readableMessage) }
                    }
                },
            )
        }
        if (!state.readOnly) TooltipIconButton(
            tooltip = stringResource(R.string.wifi_save),
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
                contentDescription = stringResource(R.string.wifi_save),
            )
        }
        qrCode?.let { value ->
            QrCodeDialog(value) { qrCode = null }
        }
    }
}

@Composable
private fun QrCodeDialog(value: String, onDismiss: () -> Unit) {
    val context = LocalContext.current
    val size = dimensionResource(R.dimen.qrcode_size)
    val density = LocalDensity.current
    val (bitmap, error) = remember(value, size, density) {
        try {
            val sizePx = with(density) { size.roundToPx() }.coerceAtLeast(1)
            val hints = mutableMapOf<EncodeHintType, Any>()
            if (!StandardCharsets.ISO_8859_1.newEncoder().canEncode(value)) {
                hints[EncodeHintType.CHARACTER_SET] = StandardCharsets.UTF_8.name()
            }
            val qrBits = MultiFormatWriter().encode(value, BarcodeFormat.QR_CODE, sizePx, sizePx, hints)
            Bitmap.createBitmap(sizePx, sizePx, Bitmap.Config.RGB_565).also {
                for (x in 0 until sizePx) for (y in 0 until sizePx) {
                    it[x, y] = if (qrBits.get(x, y)) Color.BLACK else Color.WHITE
                }
            } to null
        } catch (e: WriterException) {
            Timber.w(e)
            null to e.readableMessage
        }
    }
    LaunchedEffect(error) {
        error?.let {
            Toast.makeText(context, it, Toast.LENGTH_LONG).show()
            onDismiss()
        }
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        text = {
            if (bitmap == null) Text(stringResource(R.string.configuration_share))
            else Image(
                    bitmap = bitmap.asImageBitmap(),
                    contentDescription = stringResource(R.string.configuration_share),
                    modifier = Modifier.size(size),
                )
        },
        confirmButton = {},
    )
}
