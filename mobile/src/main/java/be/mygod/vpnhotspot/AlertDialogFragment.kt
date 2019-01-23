package be.mygod.vpnhotspot

import android.app.Activity
import android.content.DialogInterface
import android.content.Intent
import android.os.Bundle
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatDialogFragment
import androidx.versionedparcelable.ParcelUtils
import androidx.versionedparcelable.VersionedParcelable

/**
 * Based on: https://android.googlesource.com/platform/packages/apps/ExactCalculator/+/8c43f06/src/com/android/calculator2/AlertDialogFragment.java
 */
abstract class AlertDialogFragment<Arg : VersionedParcelable, Ret : VersionedParcelable> :
        AppCompatDialogFragment(), DialogInterface.OnClickListener {
    companion object {
        private const val KEY_ARG = "arg"
        private const val KEY_RET = "ret"
        fun <T : VersionedParcelable> getRet(data: Intent) =
                ParcelUtils.getVersionedParcelable<T>(data.extras, KEY_RET)!!
    }
    protected abstract fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener)

    protected val arg by lazy { ParcelUtils.getVersionedParcelable<Arg>(arguments, KEY_ARG)!! }
    protected open val ret: Ret? get() = null
    fun withArg(arg: Arg) = apply {
        arguments = Bundle().also { ParcelUtils.putVersionedParcelable(it, KEY_ARG, arg) }
    }

    override fun onCreateDialog(savedInstanceState: Bundle?): AlertDialog =
            AlertDialog.Builder(requireContext()).also { it.prepare(this) }.create()

    override fun onClick(dialog: DialogInterface?, which: Int) {
        targetFragment?.onActivityResult(targetRequestCode, which, ret?.let {
            Intent().replaceExtras(Bundle().apply { ParcelUtils.putVersionedParcelable(this, KEY_RET, it) })
        })
    }

    override fun onDismiss(dialog: DialogInterface) {
        super.onDismiss(dialog)
        targetFragment?.onActivityResult(targetRequestCode, Activity.RESULT_CANCELED, null)
    }
}

class Empty : VersionedParcelable
