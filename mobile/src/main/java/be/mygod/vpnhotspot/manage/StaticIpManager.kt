package be.mygod.vpnhotspot.manage

import android.content.DialogInterface
import android.os.Bundle
import android.os.Parcelable
import android.text.method.LinkMovementMethod
import android.view.WindowManager
import android.widget.EditText
import androidx.appcompat.app.AlertDialog
import androidx.databinding.BaseObservable
import androidx.databinding.Bindable
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.AlertDialogFragment
import be.mygod.vpnhotspot.BR
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.StaticIpSetter
import be.mygod.vpnhotspot.databinding.ListitemStaticIpBinding
import be.mygod.vpnhotspot.util.formatAddresses
import be.mygod.vpnhotspot.util.showAllowingStateLoss
import kotlinx.parcelize.Parcelize

class StaticIpManager(private val parent: TetheringFragment) : Manager(), DefaultLifecycleObserver {
    class ViewHolder(val binding: ListitemStaticIpBinding) : RecyclerView.ViewHolder(binding.root) {
        init {
            binding.text.movementMethod = LinkMovementMethod.getInstance()
        }
    }

    inner class Data : BaseObservable() {
        private var iface = StaticIpSetter.iface
        val active: Boolean @Bindable get() = iface != null
        val addresses: CharSequence @Bindable get() = iface?.formatAddresses() ?: ""

        fun onChanged() {
            iface = StaticIpSetter.iface
            notifyPropertyChanged(BR.serviceStarted)
            notifyPropertyChanged(BR.addresses)
        }

        fun configure() = ConfigureDialogFragment().apply {
            key()
            arg(ConfigureData(StaticIpSetter.ips))
        }.showAllowingStateLoss(parent.parentFragmentManager)

        fun toggle() {
            StaticIpSetter.enable(!active)
            onChanged()
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
        parent.lifecycle.addObserver(this)
        AlertDialogFragment.setResultListener<ConfigureDialogFragment, ConfigureData>(parent) { which, ret ->
            if (which == DialogInterface.BUTTON_POSITIVE) StaticIpSetter.ips = ret!!.ips.trim()
        }
    }

    override fun onCreate(owner: LifecycleOwner) {
        StaticIpSetter.ifaceEvent[this] = data::onChanged
    }

    override fun bindTo(viewHolder: RecyclerView.ViewHolder) {
        (viewHolder as ViewHolder).binding.data = data
    }

    override fun onDestroy(owner: LifecycleOwner) {
        StaticIpSetter.ifaceEvent -= this
    }
}
