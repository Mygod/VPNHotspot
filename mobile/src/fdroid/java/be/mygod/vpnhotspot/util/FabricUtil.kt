package be.mygod.vpnhotspot.util

import android.content.Context
import timber.log.Timber

object FabricUtil {
    @Suppress("UNUSED_PARAMETER")
    fun init(context: Context) {
        Timber.plant(Timber.DebugTree())
    }
}
