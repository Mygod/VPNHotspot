package be.mygod.vpnhotspot.util

import android.content.Context
import me.weishu.reflection.Reflection

fun UnblockHelper(context: Context) {
    check(Reflection.unseal(context) == 0)
}
