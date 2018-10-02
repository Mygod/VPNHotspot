package be.mygod.vpnhotspot.util

import com.crashlytics.android.Crashlytics
import timber.log.Timber

class CrashlyticsTree : Timber.Tree() {
    override fun log(priority: Int, tag: String?, message: String, t: Throwable?) {
        if (t == null)
            Crashlytics.log(priority, tag, message)
        else
            Crashlytics.logException(t)
    }
}
