package be.mygod.vpnhotspot.ui.apconfiguration

import android.net.wifi.SoftApConfiguration
import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.ui.PreferenceRow
import be.mygod.vpnhotspot.ui.PreferenceSelectionRow

@Composable
internal fun ChannelApRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    selected: ChannelOption,
    entries: List<ChannelOption>,
    description: AnnotatedString? = null,
    onSelect: (ChannelOption) -> Unit,
) {
    val context = LocalContext.current
    var selecting by rememberSaveable { mutableStateOf(false) }
    PreferenceRow(
        icon = icon,
        title = stringResource(title),
        summary = selected.label(context),
        onClick = { selecting = true },
    )
    if (selecting) ChannelApDialog(
        title = stringResource(title),
        selected = selected,
        entries = entries,
        description = description,
        onDismissRequest = { selecting = false },
        onSelect = onSelect,
    )
}

@Composable
@OptIn(ExperimentalLayoutApi::class)
private fun ChannelApDialog(
    title: String,
    selected: ChannelOption,
    entries: List<ChannelOption>,
    description: AnnotatedString?,
    onDismissRequest: () -> Unit,
    onSelect: (ChannelOption) -> Unit,
) {
    val context = LocalContext.current
    val disabledOption = entries.firstOrNull { it === ChannelOption.Disabled }
    val channelEntries = entries.filter { it !== ChannelOption.Disabled }
    val bands = BAND_ORDER.filter { band -> channelEntries.any { it.band and band == band } }
    var selectedBandMask by rememberSaveable(selected.band, selected.channel, entries.size) {
        mutableIntStateOf(if (selected.band > 0) selected.band else channelEntries.firstOrNull()?.band ?: 0)
    }
    val selectableChannels = buildList {
        channelEntries.firstOrNull { it.band == selectedBandMask && it.channel == 0 }?.let { add(it) }
        if (selectedBandMask in BAND_ORDER) addAll(channelEntries.filter {
            it.band == selectedBandMask && it.channel != 0
        })
    }
    AlertDialog(
        onDismissRequest = onDismissRequest,
        title = { Text(title) },
        text = {
            LazyColumn(
                contentPadding = PaddingValues(vertical = 8.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                description?.let {
                    item("description") {
                        Text(
                            text = it,
                            modifier = Modifier.padding(bottom = 8.dp),
                            style = MaterialTheme.typography.bodyMedium,
                        )
                    }
                }
                disabledOption?.let {
                    item("disabled") {
                        PreferenceSelectionRow(
                            index = 0,
                            count = 1,
                            selected = selected === ChannelOption.Disabled,
                            title = it.label(context),
                        ) {
                            onSelect(it)
                            onDismissRequest()
                        }
                    }
                }
                item("bands") {
                    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text(
                            text = stringResource(R.string.wifi_hotspot_ap_band_title),
                            style = MaterialTheme.typography.labelLarge,
                        )
                        FlowRow(
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                            verticalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            for (band in bands) {
                                val selectedBand = selectedBandMask and band == band
                                FilterChip(
                                    selected = selectedBand,
                                    onClick = {
                                        val next = if (selectedBand) selectedBandMask and band.inv()
                                        else selectedBandMask or band
                                        if (next != 0) selectedBandMask = next
                                    },
                                    label = { Text(ChannelOption(band).label(context)) },
                                    leadingIcon = if (selectedBand) {
                                        {
                                            Icon(
                                                imageVector = Icons.Filled.Check,
                                                contentDescription = null,
                                                modifier = Modifier.size(18.dp),
                                            )
                                        }
                                    } else null,
                                )
                            }
                        }
                    }
                }
                if (selectableChannels.isEmpty()) {
                    item("empty") {
                        Text(
                            text = stringResource(R.string.wifi_hotspot_ap_channel_band_empty),
                            style = MaterialTheme.typography.bodyMedium,
                        )
                    }
                }
                itemsIndexed(selectableChannels, key = { _, it -> "${it.band}/${it.channel}" }) { index, option ->
                    PreferenceSelectionRow(
                        index = index,
                        count = selectableChannels.size,
                        selected = selected.sameChannel(option),
                        title = if (option.channel == 0) {
                            stringResource(R.string.wifi_channel_width_auto)
                        } else option.label(context),
                        summary = if (option.channel == 0) AnnotatedString(option.label(context)) else null,
                    ) {
                        onSelect(option)
                        onDismissRequest()
                    }
                }
            }
        },
        confirmButton = {},
    )
}

private fun ChannelOption.sameChannel(other: ChannelOption) = this === ChannelOption.Disabled && other === ChannelOption.Disabled ||
        this !== ChannelOption.Disabled && other !== ChannelOption.Disabled &&
        band == other.band && channel == other.channel

private val BAND_ORDER = listOf(
    SoftApConfiguration.BAND_2GHZ,
    SoftApConfiguration.BAND_5GHZ,
    SoftApConfiguration.BAND_6GHZ,
    SoftApConfiguration.BAND_60GHZ,
)
