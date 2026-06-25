package be.mygod.vpnhotspot.widget

import android.annotation.SuppressLint
import android.os.Looper
import android.widget.Toast
import androidx.annotation.MainThread
import androidx.annotation.StringRes
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.readableMessage

class SmartSnackbar private constructor(
    private val text: CharSequence,
) {
    internal class Request(
        val text: CharSequence,
        val actionText: CharSequence?,
        val action: (() -> Unit)?,
    )

    companion object {
        private var composeHandler: ((Request) -> Boolean)? = null

        fun make(@StringRes text: Int): SmartSnackbar = make(app.getText(text))
        fun make(text: CharSequence = "") = SmartSnackbar(text)
        fun make(e: Throwable) = make(e.readableMessage)

        @MainThread
        internal fun registerComposeHandler(
            handler: (Request) -> Boolean,
        ): AutoCloseable {
            composeHandler = handler
            return AutoCloseable { if (composeHandler === handler) composeHandler = null }
        }
    }

    private var actionText: CharSequence? = null
    private var action: (() -> Unit)? = null
    private var toastDuration = Toast.LENGTH_LONG

    fun show() {
        if (Looper.myLooper() == Looper.getMainLooper()) showOnMain() else Services.mainHandler.post(::showOnMain)
    }

    @MainThread
    private fun showOnMain() {
        if (composeHandler?.invoke(Request(text, actionText, action)) != true) {
            @SuppressLint("ShowToast")
            Toast.makeText(app, text, toastDuration).show()
        }
    }

    fun action(@StringRes id: Int, listener: () -> Unit) {
        actionText = app.getText(id)
        action = listener
    }

    fun shortToast() = apply {
        toastDuration = Toast.LENGTH_SHORT
    }
}
