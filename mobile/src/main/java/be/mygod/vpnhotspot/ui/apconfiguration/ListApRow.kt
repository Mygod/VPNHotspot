package be.mygod.vpnhotspot.ui.apconfiguration

import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import be.mygod.vpnhotspot.ui.PreferenceRow
import be.mygod.vpnhotspot.ui.PreferenceSelectionSheet

@Composable
fun <T> ListApRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    selected: T?,
    entries: List<T>,
    entryLabel: (T) -> String,
    selectedLabel: String,
    description: AnnotatedString? = null,
    onSelect: (T) -> Unit,
) {
    var selecting by rememberSaveable { mutableStateOf(false) }
    PreferenceRow(
        icon = icon,
        title = stringResource(title),
        summary = selectedLabel,
        onClick = { selecting = true },
    )
    if (selecting) PreferenceSelectionSheet(
        title = stringResource(title),
        entryCount = entries.size,
        selectedIndex = entries.indexOf(selected),
        entryLabel = { entryLabel(entries[it]) },
        description = description,
        onDismissRequest = { selecting = false },
        onSelect = { onSelect(entries[it]) },
    )
}
