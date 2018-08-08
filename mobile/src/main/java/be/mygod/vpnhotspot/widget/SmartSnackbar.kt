package be.mygod.vpnhotspot.widget

import android.annotation.SuppressLint
import android.view.View
import android.widget.Toast
import androidx.annotation.StringRes
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.OnLifecycleEvent
import be.mygod.vpnhotspot.App.Companion.app
import com.google.android.material.snackbar.Snackbar

sealed class SmartSnackbar {
    companion object {
        @SuppressLint("StaticFieldLeak")
        private var holder: View? = null

        fun make(@StringRes text: Int): SmartSnackbar = make(app.getText(text))
        fun make(text: CharSequence = ""): SmartSnackbar {
            val holder = holder
            return if (holder == null) ToastWrapper(Toast.makeText(app, text, Toast.LENGTH_LONG)) else
                SnackbarWrapper(Snackbar.make(holder, text, Snackbar.LENGTH_LONG))
        }
    }

    class Register(private val view: View) : LifecycleObserver {
        init {
            (view.context as LifecycleOwner).lifecycle.addObserver(this)
        }

        @OnLifecycleEvent(Lifecycle.Event.ON_RESUME)
        fun onResume() {
            holder = view
        }
        @OnLifecycleEvent(Lifecycle.Event.ON_PAUSE)
        fun onPause() {
            if (holder === view) holder = null
        }
    }

    abstract fun show()
    open fun shortToast() = this
}

private class SnackbarWrapper(private val snackbar: Snackbar) : SmartSnackbar() {
    override fun show() = snackbar.show()
}

private class ToastWrapper(private val toast: Toast) : SmartSnackbar() {
    override fun show() {
        app.handler.post(toast::show)
    }

    override fun shortToast() = apply { toast.duration = Toast.LENGTH_SHORT }
}
