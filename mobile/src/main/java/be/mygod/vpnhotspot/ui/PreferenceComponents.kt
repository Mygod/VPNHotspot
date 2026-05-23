package be.mygod.vpnhotspot.ui

import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.WindowInsetsSides
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.only
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.PlainTooltip
import androidx.compose.material3.RadioButton
import androidx.compose.material3.SegmentedListItem
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TooltipAnchorPosition
import androidx.compose.material3.TooltipBox
import androidx.compose.material3.TooltipDefaults
import androidx.compose.material3.VerticalDivider
import androidx.compose.material3.rememberTooltipState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.key as composeKey
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.fromHtml
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.R

val PreferenceSplitControlWidth: Dp = 52.dp

@Composable
fun rememberTextFieldValueAtEnd(text: String, vararg inputs: Any?): MutableState<TextFieldValue> =
    rememberSaveable(text, *inputs, stateSaver = TextFieldValue.Saver) {
        mutableStateOf(TextFieldValue(text, TextRange(text.length)))
    }

@Composable
fun rememberDialogFocusRequester(enabled: Boolean = true): FocusRequester {
    val focusRequester = remember { FocusRequester() }
    val keyboard = LocalSoftwareKeyboardController.current
    LaunchedEffect(enabled) {
        if (enabled) {
            focusRequester.requestFocus()
            keyboard?.show()
        }
    }
    return focusRequester
}

@Composable
fun SettingsList(
    modifier: Modifier = Modifier,
    contentPadding: PaddingValues = PaddingValues(vertical = 8.dp),
    content: LazyListScope.() -> Unit,
) {
    LazyColumn(
        modifier = modifier.fillMaxSize(),
        contentPadding = contentPadding,
        content = content,
    )
}

fun LazyListScope.preferenceGroup(
    key: Any? = null,
    @StringRes title: Int? = null,
    content: PreferenceGroupScope.() -> Unit,
) {
    item(key = key ?: title) {
        PreferenceGroup(
            title = title?.let { stringResource(it) },
            content = content,
        )
    }
}

@Composable
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
fun PreferenceGroup(
    title: String? = null,
    horizontalPadding: Dp = 16.dp,
    content: PreferenceGroupScope.() -> Unit,
) {
    val items = ArrayList<@Composable () -> Unit>()
    PreferenceGroupScope(items).apply(content)
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = horizontalPadding, vertical = 4.dp),
        verticalArrangement = Arrangement.spacedBy(ListItemDefaults.SegmentedGap),
    ) {
        title?.let {
            Text(
                text = it,
                modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
                fontWeight = FontWeight.SemiBold,
            )
        }
        for (item in items) item()
    }
}

class PreferenceGroupScope(private val items: MutableList<@Composable () -> Unit>) {
    private var rowCount = 0

    fun row(key: Any? = null, content: @Composable () -> Unit) {
        val index = rowCount++
        items += {
            if (key == null) {
                CompositionLocalProvider(LocalPreferenceRowPosition provides PreferenceRowPosition(index, rowCount)) {
                    content()
                }
            } else composeKey(key) {
                CompositionLocalProvider(LocalPreferenceRowPosition provides PreferenceRowPosition(index, rowCount)) {
                    content()
                }
            }
        }
    }

    fun contentItem(key: Any? = null, content: @Composable () -> Unit) {
        items += {
            if (key == null) content() else composeKey(key) {
                content()
            }
        }
    }
}

@Composable
fun PreferenceRow(
    title: String,
    modifier: Modifier = Modifier,
    @DrawableRes icon: Int? = null,
    iconTint: Color? = null,
    summary: String? = null,
    summaryContent: (@Composable () -> Unit)? = null,
    enabled: Boolean = true,
    trailing: (@Composable () -> Unit)? = null,
    onClick: (() -> Unit)? = null,
) {
    PreferenceRow(
        titleContent = { Text(title) },
        modifier = modifier,
        summary = summary,
        summaryContent = summaryContent,
        enabled = enabled,
        iconContent = icon?.let {
            {
                Icon(
                    painter = painterResource(it),
                    contentDescription = null,
                    modifier = Modifier.size(24.dp),
                    tint = iconTint ?: LocalContentColor.current,
                )
            }
        },
        trailing = trailing,
        onClick = onClick,
    )
}

@Composable
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
fun PreferenceRow(
    titleContent: @Composable () -> Unit,
    modifier: Modifier = Modifier,
    summary: String? = null,
    summaryContent: (@Composable () -> Unit)? = null,
    enabled: Boolean = true,
    iconContent: (@Composable () -> Unit)? = null,
    trailing: (@Composable () -> Unit)? = null,
    onClick: (() -> Unit)? = null,
) {
    val position = LocalPreferenceRowPosition.current
    SegmentedListItem(
        selected = false,
        onClick = onClick ?: {},
        shapes = when {
            position == null -> ListItemDefaults.shapes()
            position.count == 1 -> ListItemDefaults.shapes(shape = MaterialTheme.shapes.large)
            else -> ListItemDefaults.segmentedShapes(position.index, position.count)
        },
        modifier = modifier.fillMaxWidth(),
        enabled = enabled,
        leadingContent = iconContent,
        trailingContent = trailing,
        supportingContent = summaryContent ?: summary?.takeIf { it.isNotEmpty() }?.let { { Text(it) } },
        verticalAlignment = Alignment.CenterVertically,
        colors = if (position == null) {
            ListItemDefaults.colors()
        } else {
            ListItemDefaults.segmentedColors(containerColor = MaterialTheme.colorScheme.surfaceContainer)
        },
    ) {
        titleContent()
    }
}

private class PreferenceRowPosition(val index: Int, val count: Int)

private val LocalPreferenceRowPosition = compositionLocalOf<PreferenceRowPosition?> { null }

@Composable
fun PreferenceSwitch(
    checked: Boolean,
    onCheckedChange: ((Boolean) -> Unit)?,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    interactionSource: MutableInteractionSource? = null,
) {
    Switch(
        checked = checked,
        onCheckedChange = onCheckedChange,
        modifier = modifier,
        thumbContent = {
            Icon(
                painter = painterResource(if (checked) R.drawable.ic_navigation_check else R.drawable.ic_navigation_close),
                contentDescription = null,
                modifier = Modifier.size(SwitchDefaults.IconSize),
            )
        },
        enabled = enabled,
        interactionSource = interactionSource,
    )
}

@Composable
fun PreferenceSplitSwitch(
    checked: Boolean,
    enabled: Boolean = true,
    onCheckedChange: (Boolean) -> Unit,
) {
    val interactionSource = remember { MutableInteractionSource() }
    Row(verticalAlignment = Alignment.CenterVertically) {
        Icon(
            painter = painterResource(R.drawable.ic_navigation_chevron_right),
            contentDescription = null,
            modifier = Modifier.padding(start = 16.dp, end = 8.dp),
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        VerticalDivider(
            modifier = Modifier.height(40.dp),
            color = MaterialTheme.colorScheme.outlineVariant,
        )
        Spacer(Modifier.width(12.dp))
        Box(
            modifier = Modifier
                .width(PreferenceSplitControlWidth)
                .height(48.dp)
                .toggleable(
                    value = checked,
                    interactionSource = interactionSource,
                    indication = null,
                    enabled = enabled,
                    role = Role.Switch,
                    onValueChange = onCheckedChange,
                ),
            contentAlignment = Alignment.Center,
        ) {
            PreferenceSwitch(
                checked = checked,
                modifier = Modifier.clearAndSetSemantics { },
                enabled = enabled,
                onCheckedChange = null,
                interactionSource = interactionSource,
            )
        }
    }
}

@Composable
@OptIn(ExperimentalMaterial3Api::class)
fun VpnHotspotModalBottomSheet(
    onDismissRequest: () -> Unit,
    content: @Composable ColumnScope.() -> Unit,
) {
    ModalBottomSheet(
        onDismissRequest = onDismissRequest,
        contentWindowInsets = { WindowInsets.safeDrawing.only(WindowInsetsSides.Top) },
        content = content,
    )
}

@Composable
fun modalBottomSheetListContentPadding() = PaddingValues(
    start = 24.dp,
    end = 24.dp,
    bottom = 24.dp + WindowInsets.navigationBars.asPaddingValues().calculateBottomPadding(),
)

@Composable
@OptIn(ExperimentalMaterial3Api::class, ExperimentalMaterial3ExpressiveApi::class)
fun PreferenceSelectionSheet(
    title: String,
    entryCount: Int,
    selectedIndex: Int,
    entryLabel: (Int) -> String,
    entrySummary: (Int) -> AnnotatedString? = { null },
    description: AnnotatedString? = null,
    onDismissRequest: () -> Unit,
    onSelect: (Int) -> Unit,
) {
    VpnHotspotModalBottomSheet(onDismissRequest = onDismissRequest) {
        Text(
            text = title,
            modifier = Modifier.padding(horizontal = 24.dp, vertical = 8.dp),
            style = MaterialTheme.typography.titleLarge,
        )
        LazyColumn(
            modifier = Modifier
                .fillMaxWidth()
                .weight(1f, fill = false),
            contentPadding = modalBottomSheetListContentPadding(),
            verticalArrangement = Arrangement.spacedBy(ListItemDefaults.SegmentedGap),
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
            items(entryCount, key = { it }) { index ->
                PreferenceSelectionRow(
                    index = index,
                    count = entryCount,
                    selected = index == selectedIndex,
                    title = entryLabel(index),
                    summary = entrySummary(index),
                ) {
                    onSelect(index)
                    onDismissRequest()
                }
            }
        }
    }
}

@Composable
fun PreferenceSelectionRow(
    index: Int,
    count: Int,
    selected: Boolean,
    title: String,
    summary: AnnotatedString? = null,
    onClick: () -> Unit,
) {
    CompositionLocalProvider(LocalPreferenceRowPosition provides PreferenceRowPosition(index, count)) {
        PreferenceRow(
            titleContent = { Text(title) },
            summaryContent = summary?.let { { Text(it) } },
            iconContent = { RadioButton(selected = selected, onClick = null) },
            onClick = onClick,
        )
    }
}

@Composable
fun annotatedStringResource(@StringRes id: Int, vararg formatArgs: Any) = AnnotatedString.fromHtml(
    if (formatArgs.isEmpty()) stringResource(id) else stringResource(id, *formatArgs),
)

@Composable
@OptIn(ExperimentalMaterial3Api::class)
fun TooltipIconButton(
    tooltip: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    content: @Composable () -> Unit,
) {
    TooltipBox(
        positionProvider = TooltipDefaults.rememberTooltipPositionProvider(TooltipAnchorPosition.Above),
        tooltip = { PlainTooltip { Text(tooltip) } },
        state = rememberTooltipState(),
    ) {
        IconButton(
            onClick = onClick,
            modifier = modifier,
            enabled = enabled,
            content = content,
        )
    }
}

@Composable
fun RowSelectionContainer(content: @Composable () -> Unit) {
    SelectionContainer(
        modifier = Modifier.clickable(
            interactionSource = remember { MutableInteractionSource() },
            indication = null,
            onClick = { },
        ),
        content = content,
    )
}
