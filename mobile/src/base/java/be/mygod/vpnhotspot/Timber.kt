package be.mygod.vpnhotspot

import android.util.Log
import be.mygod.vpnhotspot.App.Companion.app
import com.crashlytics.android.Crashlytics
import io.fabric.sdk.android.Fabric
import timber.log.Timber

fun initTimber() {
    Fabric.with(app, Crashlytics())
    Timber.plant(object : Timber.Tree() {
        override fun log(priority: Int, tag: String?, message: String, t: Throwable?) {
            if (t == null) Crashlytics.log(priority, tag, message) else {
                // Crashlytics.logException doesn't print to logcat
                if (priority >= Log.WARN || priority == Log.DEBUG) Log.println(priority, tag, message)
                if (priority >= Log.INFO) Crashlytics.logException(t)
            }
        }
    })
}

fun debugLog(tag: String?, message: String?) {
    if (BuildConfig.DEBUG) Timber.d(tag, message)
    Crashlytics.log("$tag: $message")
}
