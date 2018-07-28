package be.mygod.vpnhotspot.manage

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.view.View
import androidx.recyclerview.widget.RecyclerView
import com.crashlytics.android.Crashlytics

object ManageBar : Manager() {
    class ViewHolder(view: View) : RecyclerView.ViewHolder(view), View.OnClickListener {
        init {
            view.setOnClickListener(this)
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
            e.printStackTrace()
            Crashlytics.logException(e)
        } catch (e: ActivityNotFoundException) {
            e.printStackTrace()
            Crashlytics.logException(e)
        } catch (e: SecurityException) {
            e.printStackTrace()
            Crashlytics.logException(e)
        }
    }
}
