package be.mygod.vpnhotspot.ui

import android.graphics.Typeface
import android.text.Spanned
import android.text.style.StrikethroughSpan
import android.text.style.StyleSpan
import android.text.style.UnderlineSpan
import android.text.style.URLSpan
import androidx.annotation.DrawableRes
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Close
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.PlainTooltip
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TooltipAnchorPosition
import androidx.compose.material3.TooltipBox
import androidx.compose.material3.TooltipDefaults
import androidx.compose.material3.rememberTooltipState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositeKeyHashCode
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.NonSkippableComposable
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.currentCompositeKeyHashCode
import androidx.compose.runtime.key
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.LinkInteractionListener
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.util.launchUrl
import com.alorma.compose.settings.ui.expressive.SettingsGroup as ComposeSettingsGroup
import com.alorma.compose.settings.ui.expressive.SettingsTileScaffold

@Composable
internal fun SettingsList(modifier: Modifier = Modifier, content: LazyListScope.() -> Unit) {
    LazyColumn(
        modifier = modifier.fillMaxSize(),
        contentPadding = PaddingValues(vertical = 8.dp),
        content = content,
    )
}

@Composable
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
internal fun PreferenceGroup(
    title: String? = null,
    content: @Composable PreferenceGroupScope.() -> Unit,
) {
    val scope = remember { PreferenceGroupScope() }
    ComposeSettingsGroup(
        modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp),
        verticalArrangement = Arrangement.spacedBy(ListItemDefaults.SegmentedGap),
        title = title?.let { { Text(it, fontWeight = FontWeight.SemiBold) } },
    ) {
        scope.clear()
        scope.content()
        scope.Render()
    }
}

internal class PreferenceGroupScope internal constructor() {
    private val items = ArrayList<PreferenceGroupItem>()

    @Composable
    @NonSkippableComposable
    fun row(content: @Composable () -> Unit) {
        items += PreferenceGroupItem.Row(currentCompositeKeyHashCode, content)
    }

    @Composable
    @NonSkippableComposable
    fun contentItem(content: @Composable () -> Unit) {
        items += PreferenceGroupItem.Content(currentCompositeKeyHashCode, content)
    }

    internal fun clear() {
        items.clear()
    }

    @Composable
    internal fun Render() {
        val count = items.count { it is PreferenceGroupItem.Row }
        var index = 0
        for (item in items) when (item) {
            is PreferenceGroupItem.Row -> key(item.key) {
                CompositionLocalProvider(LocalPreferenceRowPosition provides PreferenceRowPosition(index++, count)) {
                    item.content()
                }
            }
            is PreferenceGroupItem.Content -> key(item.key) {
                item.content()
            }
        }
    }
}

@Composable
internal fun PreferenceRow(
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
internal fun PreferenceRow(
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
    SettingsTileScaffold(
        title = titleContent,
        modifier = modifier
            .fillMaxWidth()
            .alpha(if (enabled) 1f else 0.38f),
        enabled = enabled,
        onClick = onClick ?: {},
        supportingContent = summaryContent ?: summary?.takeIf { it.isNotEmpty() }?.let { { Text(it) } },
        leadingContent = iconContent,
        colors = if (position == null) {
            ListItemDefaults.colors()
        } else {
            ListItemDefaults.segmentedColors(containerColor = MaterialTheme.colorScheme.surfaceContainer)
        },
        shapes = if (position == null) {
            ListItemDefaults.shapes()
        } else {
            ListItemDefaults.segmentedShapes(position.index, position.count)
        },
        trailingContent = trailing,
    )
}

internal class PreferenceRowPosition internal constructor(val index: Int, val count: Int)

private val LocalPreferenceRowPosition = compositionLocalOf<PreferenceRowPosition?> { null }

@Composable
internal fun PreferenceSwitch(
    checked: Boolean,
    onCheckedChange: ((Boolean) -> Unit)?,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
) {
    Switch(
        checked = checked,
        onCheckedChange = onCheckedChange,
        modifier = modifier,
        thumbContent = {
            Icon(
                imageVector = if (checked) Icons.Filled.Check else Icons.Filled.Close,
                contentDescription = null,
                modifier = Modifier.size(SwitchDefaults.IconSize),
            )
        },
        enabled = enabled,
    )
}

private sealed interface PreferenceGroupItem {
    class Row(
        val key: CompositeKeyHashCode,
        val content: @Composable () -> Unit,
    ) : PreferenceGroupItem

    class Content(
        val key: CompositeKeyHashCode,
        val content: @Composable () -> Unit,
    ) : PreferenceGroupItem
}

@Composable
@OptIn(ExperimentalMaterial3Api::class)
internal fun TooltipIconButton(
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
internal fun RowSelectionContainer(content: @Composable () -> Unit) {
    SelectionContainer(
        modifier = Modifier.clickable(
            interactionSource = remember { MutableInteractionSource() },
            indication = null,
            onClick = { },
        ),
        content = content,
    )
}

@Composable
internal fun LinkedText(
    text: CharSequence,
    modifier: Modifier = Modifier,
    textDecoration: TextDecoration? = null,
) {
    val context = LocalContext.current
    val linkStyle = SpanStyle(
        color = MaterialTheme.colorScheme.primary,
        textDecoration = TextDecoration.Underline,
    )
    val linkStyles = remember(linkStyle) { TextLinkStyles(style = linkStyle) }
    val linkInteractionListener = remember(context) {
        LinkInteractionListener { link ->
            if (link is LinkAnnotation.Url) context.launchUrl(link.url)
        }
    }
    Text(
        text = remember(text, linkStyles, linkInteractionListener) {
            text.toAnnotatedString(linkStyles, linkInteractionListener)
        },
        modifier = modifier,
        textDecoration = textDecoration,
    )
}

private fun CharSequence.toAnnotatedString(
    linkStyles: TextLinkStyles,
    linkInteractionListener: LinkInteractionListener,
): AnnotatedString = buildAnnotatedString {
    append(this@toAnnotatedString.toString())
    val spanned = this@toAnnotatedString as? Spanned ?: return@buildAnnotatedString
    spanned.forEachSpan<StyleSpan> { span, start, end ->
        addStyle(SpanStyle(
            fontWeight = if (span.style == Typeface.BOLD || span.style == Typeface.BOLD_ITALIC) {
                FontWeight.Bold
            } else null,
            fontStyle = if (span.style == Typeface.ITALIC || span.style == Typeface.BOLD_ITALIC) {
                FontStyle.Italic
            } else null,
        ), start, end)
    }
    spanned.forEachSpan<UnderlineSpan> { _, start, end ->
        addStyle(SpanStyle(textDecoration = TextDecoration.Underline), start, end)
    }
    spanned.forEachSpan<StrikethroughSpan> { _, start, end ->
        addStyle(SpanStyle(textDecoration = TextDecoration.LineThrough), start, end)
    }
    spanned.forEachSpan<URLSpan> { span, start, end ->
        addLink(LinkAnnotation.Url(span.url, linkStyles, linkInteractionListener), start, end)
    }
}

private inline fun <reified T> Spanned.forEachSpan(action: (T, Int, Int) -> Unit) {
    for (span in getSpans(0, length, T::class.java)) {
        val start = getSpanStart(span)
        val end = getSpanEnd(span)
        if (start < 0 || end <= start) continue
        action(span, start, end)
    }
}
