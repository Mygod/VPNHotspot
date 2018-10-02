package be.mygod.vpnhotspot.manage

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.os.Build
import android.provider.Settings
import android.view.View
import androidx.databinding.BaseObservable
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.databinding.ListitemManageBinding
import timber.log.Timber

object ManageBar : Manager() {
    object Data : BaseObservable() {
        /**
         * It's hard to change tethering rules with Tethering hardware acceleration enabled for now.
         *
         * See also:
         *   android.provider.Settings.Global.TETHER_OFFLOAD_DISABLED
         *   https://android.googlesource.com/platform/frameworks/base/+/android-8.1.0_r1/services/core/java/com/android/server/connectivity/tethering/OffloadHardwareInterface.java#45
         *   https://android.googlesource.com/platform/hardware/qcom/data/ipacfg-mgr/+/master/msm8998/ipacm/src/IPACM_OffloadManager.cpp
         */
        val offloadEnabled get() = Build.VERSION.SDK_INT >= 27 && Settings.Global.getInt(app.contentResolver,
                "tether_offload_disabled", 0) == 0
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
            context.startActivity(Intent()
                    .setClassName("com.android.settings", "com.android.settings.Settings\$TetherSettingsActivity"))
        } catch (e: ActivityNotFoundException) {
            startAlternative(context, e)
        } catch (e: SecurityException) {
            startAlternative(context, e)
        }
    }

    private fun startAlternative(context: Context, e: Exception) {
        try {
            context.startActivity(Intent()
                    .setClassName("com.android.settings", "com.android.settings.TetherSettings"))
            Timber.w(e)
        } catch (e: ActivityNotFoundException) {
            Timber.w(e)
        } catch (e: SecurityException) {
            Timber.w(e)
        }
    }
}
