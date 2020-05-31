package be.mygod.vpnhotspot.net

import org.junit.Assert.*
import org.junit.Test

class MacAddressCompatTest {
    @Test
    fun macSerialization() {
        for (test in listOf("01:23:45:67:89:ab", "DE:AD:88:88:BE:EF")) {
            assertTrue(test.equals(MacAddressCompat.fromString(test).toString(), true))
        }
    }
}
