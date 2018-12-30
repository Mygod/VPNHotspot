package be.mygod.vpnhotspot

import android.os.Bundle
import android.util.Log
import androidx.annotation.Size
import be.mygod.vpnhotspot.App.Companion.app
import com.crashlytics.android.Crashlytics
import com.google.firebase.analytics.FirebaseAnalytics
import io.fabric.sdk.android.Fabric
import timber.log.Timber

object DebugHelper {
    private val analytics by lazy { FirebaseAnalytics.getInstance(app.deviceStorage) }

    fun init() {
        Fabric.with(app.deviceStorage, Crashlytics())
        Timber.plant(object : Timber.DebugTree() {
            override fun log(priority: Int, tag: String?, message: String, t: Throwable?) {
                if (t == null) Crashlytics.log(priority, tag, message) else {
                    // Crashlytics.logException doesn't print to logcat
                    if (priority >= Log.WARN || priority == Log.DEBUG) Log.println(priority, tag, message)
                    if (priority >= Log.INFO) Crashlytics.logException(t)
                }
            }
        })
    }

    fun log(tag: String?, message: String?) {
        if (BuildConfig.DEBUG) Timber.tag(tag).d(message) else Crashlytics.log("$tag: $message")
    }

    fun setString(key: String, value: String?) = Crashlytics.setString(key, value)

    /**
     * This method is used to log "expected" and well-handled errors, i.e. we care less about logs, etc.
     * logException is inappropriate sometimes because it flushes all logs that could be used to investigate other bugs.
     */
    fun logEvent(@Size(min = 1L, max = 40L) event: String, extras: Bundle? = null) {
        Timber.i(if (extras == null) event else "$event, extras: $extras")
        analytics.logEvent(event, extras)
    }
}
