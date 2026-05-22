package be.mygod.vpnhotspot.ui

import androidx.compose.foundation.layout.RowScope
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable

@Composable
internal fun DialogConfirmButton(
    onClick: () -> Unit,
    enabled: Boolean = true,
    content: @Composable RowScope.() -> Unit,
) = FilledTonalButton(
    onClick = onClick,
    enabled = enabled,
    content = content,
)

@Composable
internal fun DialogNeutralButton(
    onClick: () -> Unit,
    enabled: Boolean = true,
    content: @Composable RowScope.() -> Unit,
) = OutlinedButton(
    onClick = onClick,
    enabled = enabled,
    content = content,
)

@Composable
internal fun DialogDismissButton(
    onClick: () -> Unit,
    enabled: Boolean = true,
    content: @Composable RowScope.() -> Unit,
) = TextButton(
    onClick = onClick,
    enabled = enabled,
    content = content,
)
