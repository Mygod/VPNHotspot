package be.mygod.vpnhotspot

import android.content.IntentFilter

fun createIntentFilter(vararg actions: String): IntentFilter {
    val result = IntentFilter()
    actions.forEach { result.addAction(it) }
    return result
}
