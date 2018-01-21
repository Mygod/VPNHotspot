package be.mygod.vpnhotspot.net

import android.os.Build
import android.os.Bundle
import android.support.annotation.RequiresApi

/**
 * Hidden constants from ConnectivityManager and some helpers.
 */
object ConnectivityManagerHelper {
    /**
     * This is a sticky broadcast since almost forever.
     *
     * https://android.googlesource.com/platform/frameworks/base.git/+/2a091d7aa0c174986387e5d56bf97a87fe075bdb%5E%21/services/java/com/android/server/connectivity/Tethering.java
     */
    const val ACTION_TETHER_STATE_CHANGED = "android.net.conn.TETHER_STATE_CHANGED"
    @Deprecated("No longer used on Android 8+ (API 26+)")
    private const val EXTRA_ACTIVE_TETHER_LEGACY = "activeArray"
    @RequiresApi(26)
    private const val EXTRA_ACTIVE_LOCAL_ONLY = "localOnlyArray"
    @RequiresApi(26)
    private const val EXTRA_ACTIVE_TETHER = "tetherArray"

    fun getTetheredIfaces(extras: Bundle) = if (Build.VERSION.SDK_INT >= 26)
        extras.getStringArrayList(EXTRA_ACTIVE_TETHER).toSet() + extras.getStringArrayList(EXTRA_ACTIVE_LOCAL_ONLY)
    else extras.getStringArrayList(EXTRA_ACTIVE_TETHER_LEGACY).toSet()
}
