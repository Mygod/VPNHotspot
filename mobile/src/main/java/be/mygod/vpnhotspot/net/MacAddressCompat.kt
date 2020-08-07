package be.mygod.vpnhotspot.net

import android.net.MacAddress
import androidx.annotation.RequiresApi
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Compat support class for [MacAddress].
 */
inline class MacAddressCompat(val addr: Long) {
    companion object {
        private const val ETHER_ADDR_LEN = 6
        /**
         * The MacAddress zero MAC address.
         *
         * Not publicly exposed or treated specially since the OUI 00:00:00 is registered.
         * @hide
         */
        val ALL_ZEROS_ADDRESS = MacAddressCompat(0)
        val ANY_ADDRESS = MacAddressCompat(2)

        fun bytesToString(addr: ByteArray): String {
            require(addr.size == ETHER_ADDR_LEN) { addr.contentToString() + " was not a valid MAC address" }
            return addr.joinToString(":") { "%02x".format(it) }
        }
        fun bytesToString(addr: Collection<Byte>): String {
            require(addr.size == ETHER_ADDR_LEN) { addr.joinToString() + " was not a valid MAC address" }
            return addr.joinToString(":") { "%02x".format(it) }
        }

        /**
         * Creates a MacAddress from the given byte array representation.
         * A valid byte array representation for a MacAddress is a non-null array of length 6.
         *
         * @param addr a byte array representation of a MAC address.
         * @return the MacAddress corresponding to the given byte array representation.
         * @throws IllegalArgumentException if the given byte array is not a valid representation.
         */
        fun fromBytes(addr: ByteArray): MacAddressCompat {
            require(addr.size == ETHER_ADDR_LEN) { addr.joinToString() + " was not a valid MAC address" }
            return ByteBuffer.allocate(Long.SIZE_BYTES).run {
                put(addr)
                rewind()
                MacAddressCompat(long)
            }
        }
        /**
         * Creates a MacAddress from the given String representation. A valid String representation
         * for a MacAddress is a series of 6 values in the range [0,ff] printed in hexadecimal
         * and joined by ':' characters.
         *
         * @param addr a String representation of a MAC address.
         * @return the MacAddress corresponding to the given String representation.
         * @throws IllegalArgumentException if the given String is not a valid representation.
         */
        fun fromString(addr: String) = ByteBuffer.allocate(Long.SIZE_BYTES).run {
            order(ByteOrder.LITTLE_ENDIAN)
            var start = 0
            var i = 0
            while (position() < ETHER_ADDR_LEN && start < addr.length) {
                val end = i
                if (addr.getOrElse(i) { ':' } == ':') ++i else if (i < start + 2) {
                    ++i
                    continue
                }
                put(if (start == end) 0 else try {
                    Integer.parseInt(addr.substring(start, end), 16).toByte()
                } catch (e: NumberFormatException) {
                    throw IllegalArgumentException(e)
                })
                start = i
            }
            require(position() == ETHER_ADDR_LEN) { "MAC address too short" }
            rewind()
            MacAddressCompat(long)
        }

        @RequiresApi(28)
        fun MacAddress.toCompat() = fromBytes(toByteArray())
    }

    fun validate() = require(addr and ((1L shl 48) - 1).inv() == 0L)

    fun toList() = ByteBuffer.allocate(8).run {
        order(ByteOrder.LITTLE_ENDIAN)
        putLong(addr)
        array().take(6)
    }

    @RequiresApi(28)
    fun toPlatform() = MacAddress.fromBytes(toList().toByteArray())

    override fun toString() = bytesToString(toList())
}
