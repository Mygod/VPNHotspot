package be.mygod.vpnhotspot.room

import org.junit.Assert.*
import org.junit.Test

class ConvertersTest {
    @Test
    fun macSerialization() {
        for (test in listOf("01:23:45:67:89:ab", "DE:AD:88:88:BE:EF")) {
            assertTrue(test.equals(test.macToLong().macToString(), true))
        }
    }
}
