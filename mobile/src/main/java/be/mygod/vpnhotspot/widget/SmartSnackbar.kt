package be.mygod.vpnhotspot.widget

import android.annotation.SuppressLint
import android.os.Looper
import android.widget.Toast
import androidx.annotation.StringRes
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.readableMessage
import java.util.concurrent.atomic.AtomicReference

class SmartSnackbar private constructor(
    private val text: CharSequence,
    private val preferCompose: Boolean,
) {
    companion object {
        private val composeHandler = AtomicReference<((CharSequence, CharSequence?, (() -> Unit)?) -> Unit)?>()

        fun make(@StringRes text: Int): SmartSnackbar = make(app.getText(text))
        fun make(text: CharSequence = "") = SmartSnackbar(text, composeHandler.get() != null)
        fun make(e: Throwable) = make(e.readableMessage)

        fun registerComposeHandler(
            handler: (CharSequence, CharSequence?, (() -> Unit)?) -> Unit,
        ): AutoCloseable {
            composeHandler.set(handler)
            return AutoCloseable { composeHandler.compareAndSet(handler, null) }
        }
    }

    private var actionText: CharSequence? = null
    private var action: (() -> Unit)? = null
    private var toastDuration = Toast.LENGTH_LONG

    fun show() {
        val handler = if (preferCompose) composeHandler.get() else null
        if (handler == null) {
            @SuppressLint("ShowToast")
            if (Looper.myLooper() == null) Looper.prepare()
            Toast.makeText(app, text, toastDuration).show()
        } else {
            handler(text, actionText, action)
        }
    }

    fun action(@StringRes id: Int, listener: () -> Unit) {
        if (preferCompose) {
            actionText = app.getText(id)
            action = listener
        }
    }

    fun shortToast() = apply {
        if (!preferCompose) toastDuration = Toast.LENGTH_SHORT
    }
}
