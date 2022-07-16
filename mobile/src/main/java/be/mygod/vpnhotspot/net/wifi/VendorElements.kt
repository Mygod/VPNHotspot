package be.mygod.vpnhotspot.net.wifi

import android.net.wifi.ScanResult
import androidx.annotation.RequiresApi
import timber.log.Timber

@RequiresApi(33)
object VendorElements {
    fun serialize(input: List<ScanResult.InformationElement>) = input.joinToString("\n") { element ->
        element.bytes.let { buffer ->
            StringBuilder().apply {
                while (buffer.hasRemaining()) append("%02x".format(buffer.get()))
            }.toString()
        }.also {
            if (element.id != 221 || element.idExt != 0 || it.isEmpty()) Timber.w(Exception(
                "Unexpected InformationElement ${element.id}, ${element.idExt}, $it"))
        }
    }

    fun deserialize(input: CharSequence?) = (input ?: "").split("\n").map { line ->
        if (line.isBlank()) return@map null
        require(line.length % 2 == 0) { "Input should be hex: $line" }
        (0 until line.length / 2).map {
            Integer.parseInt(line.substring(it * 2, it * 2 + 2), 16).toByte()
        }.toByteArray()
    }.filterNotNull().map { ScanResult.InformationElement(221, 0, it) }
}
