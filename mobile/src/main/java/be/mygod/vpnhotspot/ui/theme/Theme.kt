package be.mygod.vpnhotspot.ui.theme

import android.os.Build
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.MotionScheme
import androidx.compose.material3.Surface
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.expressiveLightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.platform.LocalContext

@Composable
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
fun VpnHotspotTheme(dynamicColor: Boolean = true, content: @Composable () -> Unit) {
    val darkTheme = isSystemInDarkTheme()
    val colorScheme = if (dynamicColor && Build.VERSION.SDK_INT >= 31) {
        val context = LocalContext.current
        if (darkTheme) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
    } else if (darkTheme) darkColorScheme() else expressiveLightColorScheme()
    MaterialTheme(
        colorScheme = colorScheme,
        motionScheme = MotionScheme.expressive(),
        content = content,
    )
}

@Composable
internal fun VpnHotspotPreviewSurface(content: @Composable () -> Unit) {
    VpnHotspotTheme(dynamicColor = false) {
        Surface(content = content)
    }
}
