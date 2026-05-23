package be.mygod.vpnhotspot.ui.apconfiguration

import android.os.PersistableBundle
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.net.wifi.OuiKeyedData
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.nio.charset.StandardCharsets
import java.util.Locale

@RequiresApi(35)
object VendorData {
    private val header = Regex("([0-9a-fA-F]{6}):(.*)")

    fun serialize(input: List<OuiKeyedData>) = input.joinToString("\n") { data ->
        val output = ByteArrayOutputStream()
        data.data.writeToStream(output)
        String.format(Locale.ROOT, "%06x:%s", data.oui, output.toString(StandardCharsets.UTF_8.name()).trimEnd('\n'))
    }

    fun deserialize(input: CharSequence?): List<OuiKeyedData> {
        val lines = input?.toString()?.lines() ?: return emptyList()
        val result = ArrayList<OuiKeyedData>()
        var index = 0
        while (true) {
            while (index < lines.size && lines[index].isBlank()) ++index
            if (index >= lines.size) return result
            val lineNumber = index + 1
            val match = header.matchEntire(lines[index])
                ?: throw IllegalArgumentException("Expected vendor data header at line $lineNumber")
            val ouiText = match.groupValues[1]
            val oui = ouiText.toInt(16)
            require(oui != 0) { "OUI must be non-zero at line $lineNumber" }
            val xml = StringBuilder(match.groupValues[2])
            while (true) {
                try {
                    val data = ByteArrayInputStream(xml.toString().toByteArray(StandardCharsets.UTF_8)).use {
                        PersistableBundle.readFromStream(it)
                    }
                    result += OuiKeyedData(oui, data)
                    ++index
                    break
                } catch (e: IOException) {
                    ++index
                    if (index >= lines.size) {
                        throw IllegalArgumentException("Incomplete vendor data XML for OUI $ouiText", e)
                    }
                    xml.append('\n').append(lines[index])
                }
            }
        }
    }
}
