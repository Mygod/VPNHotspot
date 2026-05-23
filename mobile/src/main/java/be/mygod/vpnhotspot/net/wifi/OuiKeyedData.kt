package be.mygod.vpnhotspot.net.wifi

import android.os.Parcelable
import android.os.PersistableBundle
import androidx.annotation.RequiresApi
import kotlinx.parcelize.Parcelize

@Parcelize
@JvmInline
@RequiresApi(35)
value class OuiKeyedData(val inner: Parcelable) : Parcelable {
    companion object {
        private val clazz by lazy { Class.forName("android.net.wifi.OuiKeyedData") }
        private val getData by lazy { clazz.getDeclaredMethod("getData") }
        private val getOui by lazy { clazz.getDeclaredMethod("getOui") }
        private val builderClass by lazy { Class.forName("android.net.wifi.OuiKeyedData\$Builder") }
        private val newBuilder by lazy {
            builderClass.getConstructor(Int::class.java, PersistableBundle::class.java)
        }
        private val build by lazy { builderClass.getDeclaredMethod("build") }

        operator fun invoke(oui: Int, data: PersistableBundle) =
            OuiKeyedData(build(newBuilder.newInstance(oui, data)) as Parcelable)
    }

    val data get() = getData(inner) as PersistableBundle
    val oui get() = getOui(inner) as Int
}
