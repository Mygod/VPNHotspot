package be.mygod.vpnhotspot.root

class Jni {
    init {
        System.loadLibrary("vpnhotspot")
    }

    external fun removeUidInterfaceRules(uid: Int, rules: Long): Boolean
}
