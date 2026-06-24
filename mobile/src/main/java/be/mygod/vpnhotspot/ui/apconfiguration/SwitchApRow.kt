package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import be.mygod.vpnhotspot.ui.PreferenceSwitchRow

@Composable
fun SwitchApRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    checked: Boolean,
    summary: AnnotatedString? = null,
    onCheckedChange: (Boolean) -> Unit,
) {
    PreferenceSwitchRow(
        icon = icon,
        title = stringResource(title),
        checked = checked,
        summaryContent = summary?.let { { Text(it) } },
        onCheckedChange = onCheckedChange,
    )
}
