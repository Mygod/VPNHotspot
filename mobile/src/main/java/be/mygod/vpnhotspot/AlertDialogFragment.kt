package be.mygod.vpnhotspot

import android.app.Activity
import android.content.DialogInterface
import android.content.Intent
import android.os.Bundle
import android.os.Parcelable
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatDialogFragment
import androidx.fragment.app.Fragment
import be.mygod.vpnhotspot.util.showAllowingStateLoss
import kotlinx.android.parcel.Parcelize

/**
 * Based on: https://android.googlesource.com/platform/packages/apps/ExactCalculator/+/8c43f06/src/com/android/calculator2/AlertDialogFragment.java
 */
abstract class AlertDialogFragment<Arg : Parcelable, Ret : Parcelable> :
        AppCompatDialogFragment(), DialogInterface.OnClickListener {
    companion object {
        private const val KEY_ARG = "arg"
        private const val KEY_RET = "ret"
        fun <T : Parcelable> getRet(data: Intent) = data.getParcelableExtra<T>(KEY_RET)!!
    }
    protected abstract fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener)

    protected val arg by lazy { requireArguments().getParcelable<Arg>(KEY_ARG)!! }
    protected open val ret: Ret? get() = null
    fun withArg(arg: Arg) = apply { arguments = Bundle().apply { putParcelable(KEY_ARG, arg) } }

    override fun onCreateDialog(savedInstanceState: Bundle?): AlertDialog =
            AlertDialog.Builder(requireContext()).also { it.prepare(this) }.create()

    override fun onClick(dialog: DialogInterface?, which: Int) {
        targetFragment?.onActivityResult(targetRequestCode, which, ret?.let {
            Intent().replaceExtras(Bundle().apply { putParcelable(KEY_RET, it) })
        })
    }

    override fun onDismiss(dialog: DialogInterface) {
        super.onDismiss(dialog)
        targetFragment?.onActivityResult(targetRequestCode, Activity.RESULT_CANCELED, null)
    }

    fun show(target: Fragment, requestCode: Int = 0, tag: String = javaClass.simpleName) {
        setTargetFragment(target, requestCode)
        showAllowingStateLoss(target.parentFragmentManager, tag)
    }
}

@Parcelize
class Empty : Parcelable
