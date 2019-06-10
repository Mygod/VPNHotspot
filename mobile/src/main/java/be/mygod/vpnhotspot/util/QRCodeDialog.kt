package be.mygod.vpnhotspot.util

import android.os.Bundle
import android.view.LayoutInflater
import android.view.ViewGroup
import android.widget.ImageView
import androidx.core.os.bundleOf
import androidx.fragment.app.DialogFragment
import be.mygod.vpnhotspot.R
import net.glxn.qrgen.android.QRCode

class QRCodeDialog : DialogFragment() {
    companion object {
        private const val KEY_ARG = "arg"
    }

    fun withArg(arg: String) = apply { arguments = bundleOf(KEY_ARG to arg) }
    private val arg get() = arguments?.getString(KEY_ARG)

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?) =
            ImageView(context).apply {
                val size = resources.getDimensionPixelSize(R.dimen.qr_code_size)
                layoutParams = ViewGroup.LayoutParams(size, size)
                setImageBitmap((QRCode.from(arg).withSize(size, size) as QRCode).bitmap())
            }
}
