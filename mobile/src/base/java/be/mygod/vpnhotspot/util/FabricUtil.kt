package be.mygod.vpnhotspot.util

import android.content.Context
import be.mygod.vpnhotspot.BuildConfig
import com.crashlytics.android.Crashlytics
import io.fabric.sdk.android.Fabric
import timber.log.Timber

object FabricUtil {
    fun init(context: Context) {
        if (!BuildConfig.DEBUG) {
            Fabric.with(context, Crashlytics())
            Timber.plant(CrashlyticsTree())
        }
    }
}
