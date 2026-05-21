package be.mygod.vpnhotspot.ui.apconfiguration

import android.content.res.Configuration
import android.net.wifi.SoftApConfiguration
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.tooling.preview.Preview
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.WifiSsidCompat
import be.mygod.vpnhotspot.ui.theme.VpnHotspotPreviewSurface

@Preview(name = "Wi-Fi Configuration", showBackground = true, widthDp = 420, heightDp = 720)
@Composable
private fun ApConfigurationPreview() = ApConfigurationPreviewContent()

@Preview(
    name = "Wi-Fi Configuration - dark",
    showBackground = true,
    widthDp = 420,
    heightDp = 720,
    uiMode = Configuration.UI_MODE_NIGHT_YES,
)
@Composable
private fun ApConfigurationDarkPreview() = ApConfigurationPreviewContent()

@Composable
private fun ApConfigurationPreviewContent() {
    VpnHotspotPreviewSurface {
        ApConfigurationScreen(
            remember {
                ApConfigurationState(
                    SoftApConfigurationCompat(
                        ssid = WifiSsidCompat.fromUtf8Text("VPN Hotspot"),
                        passphrase = "12345678",
                        securityType = SoftApConfiguration.SECURITY_TYPE_WPA2_PSK,
                    ),
                    readOnly = false,
                    p2pMode = false,
                )
            },
        )
    }
}
