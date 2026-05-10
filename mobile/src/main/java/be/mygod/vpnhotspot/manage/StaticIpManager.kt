package be.mygod.vpnhotspot.manage

import android.content.DialogInterface
import android.os.Bundle
import android.os.Parcelable
import android.text.method.LinkMovementMethod
import android.view.WindowManager
import android.widget.EditText
import androidx.appcompat.app.AlertDialog
import androidx.databinding.BaseObservable
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.lifecycleScope
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.AlertDialogFragment
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.StaticIpSetter
import be.mygod.vpnhotspot.databinding.ListitemStaticIpBinding
import be.mygod.vpnhotspot.util.showAllowingStateLoss
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize

class StaticIpManager(private val parent: TetheringFragment) : Manager(), DefaultLifecycleObserver {
    class ViewHolder(val binding: ListitemStaticIpBinding) : RecyclerView.ViewHolder(binding.root) {
        init {
            binding.text.movementMethod = LinkMovementMethod.getInstance()
        }
    }

    inner class Data : BaseObservable() {
        val active: Boolean get() = StaticIpSetter.active.value
        val addresses: CharSequence get() = StaticIpSetter.addresses.value
        val applying: Boolean get() = StaticIpSetter.applying.value

        fun configure() = ConfigureDialogFragment().apply {
            key()
            arg(ConfigureData(StaticIpSetter.ips))
        }.showAllowingStateLoss(parent.parentFragmentManager)

        fun toggle() {
            StaticIpSetter.enable(!active)
            notifyChange()
        }
    }

    @Parcelize
    data class ConfigureData(val ips: String) : Parcelable
    class ConfigureDialogFragment : AlertDialogFragment<ConfigureData, ConfigureData>() {
        override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
            setTitle(R.string.tethering_static_ip)
            setView(R.layout.dialog_static_ip)
            setPositiveButton(android.R.string.ok, listener)
            setNegativeButton(android.R.string.cancel, null)
        }

        override fun onCreateDialog(savedInstanceState: Bundle?) = super.onCreateDialog(savedInstanceState).apply {
            create()
            findViewById<EditText>(android.R.id.edit)!!.setText(arg.ips)
            window!!.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_VISIBLE)
        }

        override val ret get() = ConfigureData(dialog!!.findViewById<EditText>(android.R.id.edit)!!.text!!.toString())
    }

    override val type get() = VIEW_TYPE_STATIC_IP
    private val data = Data()

    init {
        parent.viewLifecycleOwner.lifecycle.addObserver(this)
        AlertDialogFragment.setResultListener<ConfigureDialogFragment, ConfigureData>(parent) { which, ret ->
            if (which == DialogInterface.BUTTON_POSITIVE) StaticIpSetter.ips = ret!!.ips.trim()
        }
    }

    override fun onCreate(owner: LifecycleOwner) {
        owner.lifecycleScope.launch {
            merge(StaticIpSetter.active, StaticIpSetter.addresses, StaticIpSetter.applying).collect {
                data.notifyChange()
            }
        }
    }

    override fun bindTo(viewHolder: RecyclerView.ViewHolder) {
        (viewHolder as ViewHolder).binding.data = data
    }

}
