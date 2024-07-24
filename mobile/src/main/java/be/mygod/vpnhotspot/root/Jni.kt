package be.mygod.vpnhotspot.root

import android.os.Parcelable
import be.mygod.librootkotlinx.RootCommandNoResult
import dalvik.system.BaseDexClassLoader
import kotlinx.parcelize.Parcelize
import java.io.File

object Jni {
    private var initialized = 0
    private val nativeLibraryDirs by lazy {
        val pathList = BaseDexClassLoader::class.java.getDeclaredField("pathList").apply {
            isAccessible = true
        }.get(javaClass.classLoader)
        pathList.javaClass.getDeclaredField("nativeLibraryDirectories").apply {
            isAccessible = true
        }.get(pathList) as ArrayList<File>
    }

    @Parcelize
    data class Init(private val nativeDirs: List<File> = nativeLibraryDirs) : RootCommandNoResult {
        override suspend fun execute(): Parcelable? {
            if (initialized <= 0) {
                nativeLibraryDirs.addAll(nativeDirs)
                initialized = 1
            }
            if (initialized == 1) {
                System.loadLibrary("vpnhotspot")
                initialized = 2
            }
            return null
        }
    }

    external fun removeUidInterfaceRules(path: String?, uid: Int, rules: Long): Boolean
}
