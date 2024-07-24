package be.mygod.vpnhotspot.root

import androidx.annotation.Keep

class Jni {
    init {
        System.loadLibrary("vpnhotspot")
    }

    @Keep
    external fun removeUidInterfaceRules(path: String, uid: Int, rules: Long): Boolean
}
