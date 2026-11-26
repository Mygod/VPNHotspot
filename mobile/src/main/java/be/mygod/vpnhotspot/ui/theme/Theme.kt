package be.mygod.vpnhotspot.ui.theme

import android.content.Context
import android.os.Build
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.MotionScheme
import androidx.compose.material3.Surface
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.expressiveLightColorScheme
import androidx.compose.runtime.Composable
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

@Composable
fun VpnHotspotPreviewSurface(content: @Composable () -> Unit) {
    VpnHotspotTheme(dynamicColor = false) {
        Surface(content = content)
    }
}
