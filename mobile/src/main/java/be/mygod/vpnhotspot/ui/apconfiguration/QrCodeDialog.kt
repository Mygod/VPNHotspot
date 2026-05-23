package be.mygod.vpnhotspot.ui.apconfiguration

import android.graphics.Bitmap
import android.graphics.Color
import android.widget.Toast
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.size
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.core.graphics.set
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.util.readableMessage
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.MultiFormatWriter
import com.google.zxing.WriterException
import timber.log.Timber
import java.nio.charset.StandardCharsets

@Composable
fun QrCodeDialog(value: String, onDismiss: () -> Unit) {
    val context = LocalContext.current
    val size = 264.dp
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
