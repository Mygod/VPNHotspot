package be.mygod.vpnhotspot.ui

import android.net.MacAddress
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withLink
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.MacAddressCompat.Companion.toOui36String
import be.mygod.vpnhotspot.util.isBogon
import java.net.InetAddress

@Composable
fun rememberNetworkAddressLinkStyles(): TextLinkStyles {
    val style = SpanStyle(
        color = MaterialTheme.colorScheme.primary,
        textDecoration = TextDecoration.Underline,
    )
    return remember(style) { TextLinkStyles(style = style) }
}

fun AnnotatedString.Builder.appendMacAddress(mac: String, linkStyles: TextLinkStyles) {
    val prefix = try {
        MacAddress.fromString(mac).toOui36String()
    } catch (_: IllegalArgumentException) {
        return append(mac)
    }
    appendLinkedText(mac, "https://macaddress.io/macaddress/$prefix", linkStyles)
}

fun AnnotatedString.Builder.appendIpAddress(ip: InetAddress, linkStyles: TextLinkStyles) {
    val address = ip.hostAddress ?: ip.toString()
    if (ip.isBogon) append(address) else appendLinkedText(address, "https://ipinfo.io/$address", linkStyles)
}

private fun AnnotatedString.Builder.appendLinkedText(text: String, url: String, linkStyles: TextLinkStyles) {
    if (app.hasTouch) withLink(LinkAnnotation.Url(url, linkStyles)) { append(text) } else append(text)
}
