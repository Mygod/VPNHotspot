package be.mygod.vpnhotspot.net

import android.net.MacAddress
import android.os.Parcelable
import androidx.annotation.RequiresApi
import kotlinx.android.parcel.Parcelize
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Compat support class for [MacAddress].
 */
@Parcelize
inline class MacAddressCompat(val addr: Long) : Parcelable {
    companion object {
        private const val ETHER_ADDR_LEN = 6
        /**
         * The MacAddress zero MAC address.
         *
         * Not publicly exposed or treated specially since the OUI 00:00:00 is registered.
         * @hide
         */
        val ALL_ZEROS_ADDRESS = MacAddressCompat(0)
        val ANY_ADDRESS = MacAddressCompat(2L shl 40)

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
            try {
                put(addr.split(':').map { Integer.parseInt(it, 16).toByte() }.toByteArray())
            } catch (e: NumberFormatException) {
                throw IllegalArgumentException(e)
            }
            rewind()
            MacAddressCompat(long)
        }

        @RequiresApi(28)
        fun MacAddress.toCompat() = fromBytes(toByteArray())
    }

    fun toList() = ByteBuffer.allocate(8).run {
        order(ByteOrder.LITTLE_ENDIAN)
        putLong(addr)
        array().take(6)
    }

    @RequiresApi(28)
    fun toPlatform() = MacAddress.fromBytes(toList().toByteArray())

    override fun toString() = bytesToString(toList())
}
