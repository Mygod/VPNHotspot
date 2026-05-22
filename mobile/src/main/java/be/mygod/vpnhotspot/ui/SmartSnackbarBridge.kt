package be.mygod.vpnhotspot.ui

import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.rememberCoroutineScope
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
    val lifecycleOwner = LocalLifecycleOwner.current
    val scope = rememberCoroutineScope()
    DisposableEffect(snackbarHostState, lifecycleOwner, scope) {
        var registration: AutoCloseable? = null
        fun register() {
            if (registration != null) return
            registration = SmartSnackbar.registerComposeHandler { text, actionText, action ->
                scope.launch {
                    val result = snackbarHostState.showSnackbar(
                        message = text.toString(),
                        actionLabel = actionText?.toString(),
                        duration = SnackbarDuration.Long,
                    )
                    if (result == SnackbarResult.ActionPerformed) action?.invoke()
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
