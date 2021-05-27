package be.mygod.vpnhotspot.util

import android.annotation.SuppressLint
import androidx.annotation.RequiresApi
import timber.log.Timber

/**
 * The central object for accessing all the useful blocked APIs. Thanks Google!
 */
@SuppressLint("DiscouragedPrivateApi")
@Suppress("FunctionName")
object UnblockCentral {
    /**
     * Retrieve this property before doing dangerous shit.
     */
    @get:RequiresApi(28)
    private val init by lazy {
        try {
            Class.forName("dalvik.system.VMDebug").getDeclaredMethod("allowHiddenApiReflectionFrom", Class::class.java)
                .invoke(null, UnblockCentral::class.java)
            true
        } catch (e: ReflectiveOperationException) {
            Timber.w(e)
            false
        }
    }

    @RequiresApi(31)
    fun getApInstanceIdentifier(clazz: Class<*>) = init.let { clazz.getDeclaredMethod("getApInstanceIdentifier") }
}
