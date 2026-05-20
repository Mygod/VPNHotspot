package be.mygod.vpnhotspot.ui

import android.view.View
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.platform.LocalContext
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.launch

@Composable
internal fun SmartSnackbarBridge(snackbarHostState: SnackbarHostState) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    DisposableEffect(snackbarHostState, context, scope) {
        val registration = SmartSnackbar.registerComposeHandler { request ->
            scope.launch {
                val result = snackbarHostState.showSnackbar(
                    message = request.text.toString(),
                    actionLabel = request.actionText?.toString(),
                )
                if (result == SnackbarResult.ActionPerformed) request.action?.invoke(View(context))
            }
        }
        onDispose { registration.close() }
    }
}
