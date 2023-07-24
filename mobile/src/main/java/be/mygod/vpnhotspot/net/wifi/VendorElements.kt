package be.mygod.vpnhotspot.net.wifi

import android.net.wifi.ScanResult
import androidx.annotation.RequiresApi
import timber.log.Timber

@RequiresApi(33)
object VendorElements {
    fun serialize(input: List<ScanResult.InformationElement>) = input.joinToString("\n") { element ->
        element.bytes.let { buffer ->
            StringBuilder().apply {
                @OptIn(ExperimentalStdlibApi::class)
                while (buffer.hasRemaining()) append(buffer.get().toHexString())
            }.toString()
        }.also {
            if (element.id != 221 || element.idExt != 0 || it.isEmpty()) Timber.w(Exception(
                "Unexpected InformationElement ${element.id}, ${element.idExt}, $it"))
        }
    }

    fun deserialize(input: CharSequence?) = (input ?: "").split("\n").mapNotNull { line ->
        @OptIn(ExperimentalStdlibApi::class)
        if (line.isBlank()) null else ScanResult.InformationElement(221, 0, line.hexToByteArray())
    }
}
