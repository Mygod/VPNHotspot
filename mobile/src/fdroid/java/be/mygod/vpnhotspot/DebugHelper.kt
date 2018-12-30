package be.mygod.vpnhotspot

import android.os.Bundle
import androidx.annotation.Size
import timber.log.Timber

object DebugHelper {
    fun init() = Timber.plant(Timber.DebugTree())
    fun log(tag: String?, message: String?) = Timber.tag(tag).d(message)
    fun setString(key: String, value: String?) = Timber.tag(key).d(value)
    fun logEvent(@Size(min = 1L, max = 40L) event: String, extras: Bundle? = null) =
            Timber.tag("logEvent").d("$event: $extras")
}
