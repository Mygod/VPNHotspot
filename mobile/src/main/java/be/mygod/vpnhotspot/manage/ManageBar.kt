package be.mygod.vpnhotspot.manage

import android.content.ActivityNotFoundException
import android.content.Intent
import android.support.v7.widget.RecyclerView
import android.view.View

object ManageBar : Manager() {
    class ViewHolder(view: View) : RecyclerView.ViewHolder(view), View.OnClickListener {
        init {
            view.setOnClickListener(this)
        }

        override fun onClick(v: View?) = try {
            itemView.context.startActivity(Intent()
                    .setClassName("com.android.settings", "com.android.settings.Settings\$TetherSettingsActivity"))
        } catch (e: ActivityNotFoundException) {
            itemView.context.startActivity(Intent()
                    .setClassName("com.android.settings", "com.android.settings.TetherSettings"))
        }
    }

    override val type: Int get() = VIEW_TYPE_MANAGE
}
