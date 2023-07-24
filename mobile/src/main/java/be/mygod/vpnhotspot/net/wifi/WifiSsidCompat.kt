package be.mygod.vpnhotspot.net.wifi

import android.net.wifi.WifiSsid
import android.os.Parcelable
import androidx.annotation.RequiresApi
import kotlinx.parcelize.Parcelize
import org.jetbrains.annotations.Contract
import java.nio.ByteBuffer
import java.nio.CharBuffer
import java.nio.charset.Charset
import java.nio.charset.CodingErrorAction

@Parcelize
data class WifiSsidCompat(val bytes: ByteArray) : Parcelable {
    companion object {
        private val hexTester = Regex("^(?:[0-9a-f]{2})*$", RegexOption.IGNORE_CASE)
        private val qrSanitizer = Regex("([\\\\\":;,])")

        @OptIn(ExperimentalStdlibApi::class)
        fun fromHex(hex: String?) = hex?.run { WifiSsidCompat(hexToByteArray()) }

        @Contract("null -> null; !null -> !null")
        fun fromUtf8Text(text: String?, truncate: Boolean = false) = text?.toByteArray()?.let {
            WifiSsidCompat(if (truncate && it.size > 32) it.sliceArray(0 until 32) else it)
        }

        fun toMeCard(text: String) = qrSanitizer.replace(text) { "\\${it.groupValues[1]}" }

        @RequiresApi(33)
        fun WifiSsid.toCompat() = WifiSsidCompat(bytes)
    }

    init {
        require(bytes.size <= 32) { "${bytes.size} > 32" }
    }

    @RequiresApi(31)
    fun toPlatform() = WifiSsid.fromBytes(bytes)

    fun decode(charset: Charset = Charsets.UTF_8) = CharBuffer.allocate(32).run {
        val result = charset.newDecoder().apply {
            onMalformedInput(CodingErrorAction.REPORT)
            onUnmappableCharacter(CodingErrorAction.REPORT)
        }.decode(ByteBuffer.wrap(bytes), this, true)
        if (result.isError) null else flip().toString()
    }
    @OptIn(ExperimentalStdlibApi::class)
    val hex get() = bytes.toHexString()

    fun toMeCard(): String {
        val utf8 = decode() ?: return hex
        return if (hexTester.matches(utf8)) "\"$utf8\"" else toMeCard(utf8)
    }

    override fun toString() = String(bytes)

    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false
        other as WifiSsidCompat
        if (!bytes.contentEquals(other.bytes)) return false
        return true
    }

    override fun hashCode() = bytes.contentHashCode()
}
