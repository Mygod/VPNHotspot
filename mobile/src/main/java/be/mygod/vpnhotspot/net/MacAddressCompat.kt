package be.mygod.vpnhotspot.net

import android.net.MacAddress
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * This used to be a compat support class for [MacAddress].
 * Now it is just a convenient class for backwards compatibility.
 */
@JvmInline
value class MacAddressCompat(val addr: Long) {
    companion object {
        /**
         * The MacAddress zero MAC address.
         *
         * Not publicly exposed or treated specially since the OUI 00:00:00 is registered.
         */
        val ALL_ZEROS_ADDRESS = MacAddress.fromBytes(byteArrayOf(0, 0, 0, 0, 0, 0))
        val ANY_ADDRESS = MacAddress.fromBytes(byteArrayOf(2, 0, 0, 0, 0, 0))

        fun MacAddress.toLong() = ByteBuffer.allocate(Long.SIZE_BYTES).apply {
            order(ByteOrder.LITTLE_ENDIAN)
            put(toByteArray())
            rewind()
        }.long
    }

    fun toPlatform() = MacAddress.fromBytes(ByteBuffer.allocate(8).run {
        order(ByteOrder.LITTLE_ENDIAN)
        putLong(addr)
        array().take(6)
    }.toByteArray())
}
