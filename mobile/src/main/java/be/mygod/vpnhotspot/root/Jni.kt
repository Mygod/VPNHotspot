package be.mygod.vpnhotspot.root

object Jni {
    init {
        System.loadLibrary("vpnhotspot")
    }
    external fun removeUidInterfaceRules(path: String?, uid: Int, rules: Long): Boolean
}
