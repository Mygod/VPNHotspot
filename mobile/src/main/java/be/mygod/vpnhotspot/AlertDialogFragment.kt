package be.mygod.vpnhotspot

import android.app.Activity
import android.content.DialogInterface
import android.content.Intent
import android.os.Bundle
import androidx.appcompat.app.AlertDialog
import androidx.fragment.app.DialogFragment

abstract class AlertDialogFragment : DialogFragment(), DialogInterface.OnClickListener {
    protected abstract fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener)
    open val data: Intent? get() = null

    override fun onCreateDialog(savedInstanceState: Bundle?): AlertDialog =
            AlertDialog.Builder(requireContext()).also { it.prepare(this) }.create()

    override fun onClick(dialog: DialogInterface?, which: Int) {
        targetFragment!!.onActivityResult(targetRequestCode, which, data)
    }

    override fun onDismiss(dialog: DialogInterface?) {
        super.onDismiss(dialog)
        targetFragment!!.onActivityResult(targetRequestCode, Activity.RESULT_CANCELED, data)
    }
}
