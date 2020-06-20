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
    companion object {
        private val holder = AtomicReference<View?>()

        fun make(@StringRes text: Int): SmartSnackbar = make(app.getText(text))
        fun make(text: CharSequence = ""): SmartSnackbar {
            val holder = holder.get()
            return if (holder == null) @SuppressLint("ShowToast") {
                if (Looper.myLooper() == null) Looper.prepare()
                ToastWrapper(Toast.makeText(app, text, Toast.LENGTH_LONG))
            } else SnackbarWrapper(Snackbar.make(holder, text, Snackbar.LENGTH_LONG))
        }
        fun make(e: Throwable) = make(e.readableMessage)
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
