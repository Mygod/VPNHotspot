package be.mygod.vpnhotspot.ui.theme

import android.content.Context
import android.os.Build
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.MotionScheme
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Surface
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.expressiveLightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext

fun vpnHotspotColorScheme(context: Context, darkTheme: Boolean, dynamicColor: Boolean = true) =
    if (dynamicColor && Build.VERSION.SDK_INT >= 31) {
        if (darkTheme) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
    } else if (darkTheme) darkColorScheme() else expressiveLightColorScheme()

@Composable
fun VpnHotspotTheme(dynamicColor: Boolean = true, content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = vpnHotspotColorScheme(LocalContext.current, isSystemInDarkTheme(), dynamicColor),
        motionScheme = MotionScheme.expressive(),
        content = content,
    )
}

/**
 * [OutlinedTextFieldDefaults.tonalColors] with transparent containers. material3 1.5.0 fills them with onPrimary under
 * onSurface text, which hides the text when a ROM's dynamic colors make onPrimary light in dark theme:
 * https://github.com/Mygod/VPNHotspot/issues/821
 * androidx.compose.material3:material3 1.6.0-alpha01 makes the same change ("Changed the container color in
 * `OutlinedTextFieldDefaults.tonalColors` to transparent"), following the latest Expressive design:
 * https://github.com/androidx/androidx/commit/f06c082512ae989f08fc4f357cd55e889d7e6ae3
 * Use [OutlinedTextFieldDefaults.tonalColors] directly once the Compose BOM resolves material3 1.6.0 or newer.
 */
@Composable
fun tonalOutlinedTextFieldColors() = OutlinedTextFieldDefaults.tonalColors().copy(
    focusedContainerColor = Color.Transparent,
    unfocusedContainerColor = Color.Transparent,
    disabledContainerColor = Color.Transparent,
    errorContainerColor = Color.Transparent,
)

@Composable
fun VpnHotspotPreviewSurface(content: @Composable () -> Unit) {
    VpnHotspotTheme(dynamicColor = false) {
        Surface(content = content)
    }
}
