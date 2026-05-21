package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import be.mygod.vpnhotspot.ui.PreferenceRow
import be.mygod.vpnhotspot.ui.PreferenceSwitch

@Composable
internal fun SwitchApRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    checked: Boolean,
    readOnly: Boolean,
    summary: AnnotatedString? = null,
    onCheckedChange: (Boolean) -> Unit,
) {
    PreferenceRow(
        icon = icon,
        title = stringResource(title),
        summaryContent = summary?.let { { Text(it) } },
        enabled = !readOnly,
        trailing = {
            PreferenceSwitch(
                checked = checked,
                enabled = !readOnly,
                onCheckedChange = if (readOnly) null else onCheckedChange,
            )
        },
        onClick = { onCheckedChange(!checked) },
    )
}
