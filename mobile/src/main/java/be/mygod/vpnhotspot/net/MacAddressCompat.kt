package be.mygod.vpnhotspot.net

import java.nio.ByteBuffer
import java.nio.ByteOrder

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

        fun bytesToString(addr: ByteArray): String {
            require(addr.size == ETHER_ADDR_LEN) { addr.contentToString() + " was not a valid MAC address" }
            return addr.joinToString(":") { "%02x".format(it) }
        }
        fun bytesToString(addr: Collection<Byte>): String {
            require(addr.size == ETHER_ADDR_LEN) { addr.joinToString() + " was not a valid MAC address" }
            return addr.joinToString(":") { "%02x".format(it) }
        }

        @Throws(IllegalArgumentException::class)
        fun fromString(addr: String) = MacAddressCompat(ByteBuffer.allocate(8).run {
            order(ByteOrder.LITTLE_ENDIAN)
            mark()
            try {
                put(addr.split(':').map { Integer.parseInt(it, 16).toByte() }.toByteArray())
            } catch (e: NumberFormatException) {
                throw IllegalArgumentException(e)
            }
            reset()
            long
        })
    }

    override fun toString() = ByteBuffer.allocate(8).run {
        order(ByteOrder.LITTLE_ENDIAN)
        putLong(addr)
        bytesToString(array().take(6))
    }
}
