package be.mygod.librootkotlinx

import android.util.Log

interface Logger {
    companion object {
        /**
         * Override this variable to change default behavior,
         * which is to print to [android.util.Log] under tag "RootServer" except for [d].
         */
        @JvmStatic
        var me = object : Logger { }

        private const val TAG = "RootServer"
    }

    fun d(m: String?, t: Throwable? = null) { }
    fun e(m: String?, t: Throwable? = null) {
        Log.e(TAG, m, t)
    }
    fun i(m: String?, t: Throwable? = null) {
        Log.i(TAG, m, t)
    }
    fun w(m: String?, t: Throwable? = null) {
        Log.w(TAG, m, t)
    }
}
