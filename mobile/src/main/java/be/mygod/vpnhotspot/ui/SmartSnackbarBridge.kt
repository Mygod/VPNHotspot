package be.mygod.vpnhotspot.ui

import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.compose.LocalLifecycleOwner
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.channels.Channel

suspend fun SnackbarHostState.showLongSnackbar(message: String) {
    showSnackbar(message = message, duration = SnackbarDuration.Long)
}

@Composable
fun SmartSnackbarBridge(snackbarHostState: SnackbarHostState) {
    val lifecycleOwner = LocalLifecycleOwner.current
    val requests = remember { Channel<SmartSnackbar.Request>(Channel.UNLIMITED) }
    LaunchedEffect(snackbarHostState, requests) {
        for (request in requests) {
            val result = snackbarHostState.showSnackbar(
                message = request.text.toString(),
                actionLabel = request.actionText?.toString(),
                duration = SnackbarDuration.Long,
            )
            if (result == SnackbarResult.ActionPerformed) request.action?.invoke()
        }
    }
    DisposableEffect(requests) {
        onDispose { requests.close() }
    }
    DisposableEffect(lifecycleOwner, requests) {
        var registration: AutoCloseable? = null
        fun register() {
            if (registration != null) return
            registration = SmartSnackbar.registerComposeHandler { request ->
                requests.trySend(request).isSuccess
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
