package be.mygod.vpnhotspot.util

import android.app.Activity

interface AppUpdate {
    val downloaded: Boolean? get() = null
    val message: String? get() = null
    val stalenessDays: Int? get() = null
    fun updateForResult(activity: Activity, requestCode: Int): Unit = error("Update not supported")
}
