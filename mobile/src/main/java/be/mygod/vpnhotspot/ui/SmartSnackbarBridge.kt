package be.mygod.vpnhotspot.ui

import android.view.View
import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.compose.LocalLifecycleOwner
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.launch

suspend fun SnackbarHostState.showLongSnackbar(message: String) {
    showSnackbar(message = message, duration = SnackbarDuration.Long)
}

@Composable
fun SmartSnackbarBridge(snackbarHostState: SnackbarHostState) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val scope = rememberCoroutineScope()
    DisposableEffect(snackbarHostState, context, lifecycleOwner, scope) {
        var registration: AutoCloseable? = null
        fun register() {
            if (registration != null) return
            registration = SmartSnackbar.registerComposeHandler { request ->
                scope.launch {
                    val result = snackbarHostState.showSnackbar(
                        message = request.text.toString(),
                        actionLabel = request.actionText?.toString(),
                        duration = SnackbarDuration.Long,
                    )
                    if (result == SnackbarResult.ActionPerformed) request.action?.invoke(View(context))
                }
            }
        }
        fun unregister() {
            registration?.close()
            registration = null
        }
        val observer = object : DefaultLifecycleObserver {
            override fun onResume(owner: LifecycleOwner) = register()
            override fun onPause(owner: LifecycleOwner) = unregister()
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        if (lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)) register()
        onDispose {
            lifecycleOwner.lifecycle.removeObserver(observer)
            unregister()
        }
    }
}
