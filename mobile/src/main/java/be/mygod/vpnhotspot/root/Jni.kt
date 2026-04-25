package be.mygod.vpnhotspot.root

object Jni {
    init {
        System.loadLibrary("vpnhotspot")
    }
    external fun launchProcess(command: Array<String>, stdin: Int, stdout: Int, stderr: Int)
    external fun removeUidInterfaceRules(path: String?, uid: Int, rules: Long): Boolean
}
