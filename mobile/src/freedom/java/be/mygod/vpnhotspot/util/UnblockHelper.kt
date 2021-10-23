package be.mygod.vpnhotspot.util

import android.content.Context
import java.lang.reflect.Method

fun UnblockHelper(context: Context) {
    val getDeclaredMethod = Class::class.java.getDeclaredMethod("getDeclaredMethod",
        String::class.java, arrayOf<Class<*>>()::class.java)
    val clazz = Class.forName("dalvik.system.VMRuntime")
    val setHiddenApiExemptions = getDeclaredMethod(clazz, "setHiddenApiExemptions",
        arrayOf(Array<String>::class.java)) as Method
    setHiddenApiExemptions(clazz.getDeclaredMethod("getRuntime")(null), arrayOf(""))
}
