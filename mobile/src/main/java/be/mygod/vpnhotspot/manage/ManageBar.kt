package be.mygod.vpnhotspot.manage

import android.content.Context
import android.content.Intent
import android.os.Build
import android.view.View
import androidx.core.os.bundleOf
import androidx.databinding.BaseObservable
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.DebugHelper
import be.mygod.vpnhotspot.databinding.ListitemManageBinding
import be.mygod.vpnhotspot.net.TetherOffloadManager

object ManageBar : Manager() {
    private const val TAG = "ManageBar"
    private const val SETTINGS_PACKAGE = "com.android.settings"
    private const val SETTINGS_1 = "com.android.settings.Settings\$TetherSettingsActivity"
    private const val SETTINGS_2 = "com.android.settings.TetherSettings"

    object Data : BaseObservable() {
        val offloadEnabled get() = Build.VERSION.SDK_INT >= 27 && TetherOffloadManager.enabled
    }
    class ViewHolder(binding: ListitemManageBinding) : RecyclerView.ViewHolder(binding.root), View.OnClickListener {
        init {
            binding.data = Data
            binding.root.setOnClickListener(this)
        }

        override fun onClick(v: View?) = start(itemView.context)
    }

    override val type: Int get() = VIEW_TYPE_MANAGE

    fun start(context: Context) {
        try {
            context.startActivity(Intent().setClassName(SETTINGS_PACKAGE, SETTINGS_1))
        } catch (e1: RuntimeException) {
            try {
                context.startActivity(Intent().setClassName(SETTINGS_PACKAGE, SETTINGS_2))
                DebugHelper.logEvent(TAG, bundleOf(Pair(SETTINGS_1, e1.message)))
            } catch (e2: RuntimeException) {
                DebugHelper.logEvent(TAG, bundleOf(Pair(SETTINGS_1, e1.message), Pair(SETTINGS_2, e2.message)))
            }
        }
    }
}
