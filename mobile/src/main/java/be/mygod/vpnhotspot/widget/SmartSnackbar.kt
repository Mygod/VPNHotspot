package be.mygod.vpnhotspot.widget

import android.annotation.SuppressLint
import android.os.Looper
import android.view.View
import android.widget.Toast
import androidx.annotation.StringRes
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.findViewTreeLifecycleOwner
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.readableMessage
import com.google.android.material.snackbar.Snackbar
import java.util.concurrent.atomic.AtomicReference

sealed class SmartSnackbar {
    data class ComposeRequest(
        val text: CharSequence,
        val actionText: CharSequence?,
        val action: ((View) -> Unit)?,
    )

    companion object {
        private val holder = AtomicReference<View?>()
        private val composeRegistration = AtomicReference<ComposeRegistration?>()

        fun make(@StringRes text: Int): SmartSnackbar = make(app.getText(text))
        fun make(text: CharSequence = ""): SmartSnackbar {
            val holder = holder.get()
            return when {
                holder != null -> SnackbarWrapper(Snackbar.make(holder, text, Snackbar.LENGTH_LONG))
                composeRegistration.get() != null -> ComposeWrapper(text)
                else -> @SuppressLint("ShowToast") {
                    if (Looper.myLooper() == null) Looper.prepare()
                    ToastWrapper(Toast.makeText(app, text, Toast.LENGTH_LONG))
                }
            }
        }
        fun make(e: Throwable) = make(e.readableMessage)

        fun registerComposeHandler(handler: (ComposeRequest) -> Unit): AutoCloseable {
            val registration = ComposeRegistration(handler)
            composeRegistration.set(registration)
            return AutoCloseable { composeRegistration.compareAndSet(registration, null) }
        }

        private class ComposeRegistration(val handler: (ComposeRequest) -> Unit)

        fun sendToCompose(request: ComposeRequest): Boolean {
            val registration = composeRegistration.get() ?: return false
            registration.handler(request)
            return true
        }
    }

    class Register(private val view: View) : DefaultLifecycleObserver {
        init {
            view.findViewTreeLifecycleOwner()!!.lifecycle.addObserver(this)
        }

        override fun onResume(owner: LifecycleOwner) = holder.set(view)
        override fun onPause(owner: LifecycleOwner) {
            holder.compareAndSet(view, null)
        }
    }

    abstract fun show()
    open fun action(@StringRes id: Int, listener: (View) -> Unit) { }
    open fun shortToast() = this
}

private class ComposeWrapper(private val text: CharSequence) : SmartSnackbar() {
    private var actionText: CharSequence? = null
    private var action: ((View) -> Unit)? = null

    override fun show() {
        if (!SmartSnackbar.sendToCompose(SmartSnackbar.ComposeRequest(text, actionText, action))) {
            @SuppressLint("ShowToast")
            if (Looper.myLooper() == null) Looper.prepare()
            Toast.makeText(app, text, Toast.LENGTH_LONG).show()
        }
    }

    override fun action(@StringRes id: Int, listener: (View) -> Unit) {
        actionText = app.getText(id)
        action = listener
    }
}

private class SnackbarWrapper(private val snackbar: Snackbar) : SmartSnackbar() {
    override fun show() = snackbar.show()

    override fun action(@StringRes id: Int, listener: (View) -> Unit) {
        snackbar.setAction(id, listener)
    }
}

private class ToastWrapper(private val toast: Toast) : SmartSnackbar() {
    override fun show() = toast.show()

    override fun shortToast() = apply { toast.duration = Toast.LENGTH_SHORT }
}
