package be.mygod.vpnhotspot.widget

import android.annotation.SuppressLint
import android.os.Looper
import android.view.View
import android.widget.Toast
import androidx.annotation.StringRes
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleObserver
import androidx.lifecycle.OnLifecycleEvent
import be.mygod.vpnhotspot.App.Companion.app
import com.google.android.material.snackbar.Snackbar
import com.topjohnwu.superuser.NoShellException

sealed class SmartSnackbar {
    companion object {
        @SuppressLint("StaticFieldLeak")
        private var holder: View? = null

        fun make(@StringRes text: Int): SmartSnackbar = make(app.getText(text))
        fun make(text: CharSequence? = ""): SmartSnackbar {
            val holder = holder
            return if (holder == null) @SuppressLint("ShowToast") {
                if (Looper.myLooper() == null) Looper.prepare()
                ToastWrapper(Toast.makeText(app, text, Toast.LENGTH_LONG))
            } else SnackbarWrapper(Snackbar.make(holder, text ?: null.toString(), Snackbar.LENGTH_LONG))
        }
        fun make(e: Throwable) = make(when (e) {
            is NoShellException -> e.cause ?: e
            else -> e
        }.localizedMessage)
    }

    class Register(lifecycle: Lifecycle, private val view: View) : LifecycleObserver {
        init {
            lifecycle.addObserver(this)
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
    override fun show() = toast.show()

    override fun shortToast() = apply { toast.duration = Toast.LENGTH_SHORT }
}
