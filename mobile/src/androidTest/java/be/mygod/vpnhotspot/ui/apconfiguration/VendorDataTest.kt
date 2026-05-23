package be.mygod.vpnhotspot.ui.apconfiguration

import android.os.Build
import android.os.PersistableBundle
import be.mygod.vpnhotspot.net.wifi.OuiKeyedData
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Test

class VendorDataTest {
    @Before
    fun assumeVendorDataSupported() {
        assumeTrue(Build.VERSION.SDK_INT >= 35)
    }

    @Test
    fun roundTripPreservesOrderAndDuplicateOui() {
        val source = listOf(
            OuiKeyedData(0x00aabb, PersistableBundle().apply { putString("first", "value") }),
            OuiKeyedData(0x00aabb, PersistableBundle().apply { putInt("second", 2) }),
            OuiKeyedData(0x00aacc, PersistableBundle().apply { putBoolean("third", true) }),
        )

        val parsed = VendorData.deserialize(VendorData.serialize(source))

        assertEquals(listOf(0x00aabb, 0x00aabb, 0x00aacc), parsed.map { it.oui })
        assertEquals("value", parsed[0].data.getString("first"))
        assertEquals(2, parsed[1].data.getInt("second"))
        assertEquals(true, parsed[2].data.getBoolean("third"))
    }

    @Test
    fun parsesMultilineAndSingleLineXml() {
        val parsed = VendorData.deserialize("""
            00aabb:<bundle>
            <string name="foo">bar</string>
            </bundle>

            00aacc:<bundle><boolean name="flag" value="true" /></bundle>
        """.trimIndent())

        assertEquals(listOf(0x00aabb, 0x00aacc), parsed.map { it.oui })
        assertEquals("bar", parsed[0].data.getString("foo"))
        assertEquals(true, parsed[1].data.getBoolean("flag"))
    }

    @Test
    fun preservesEscapedStringCharactersAndArrays() {
        val source = PersistableBundle().apply {
            putString("text", "a\nb\rc\td < &")
            putIntArray("ints", intArrayOf(1, 2, 3))
        }

        val parsed = VendorData.deserialize(VendorData.serialize(listOf(OuiKeyedData(0x00aabb, source)))).single()

        assertEquals("a\nb\rc\td < &", parsed.data.getString("text"))
        assertArrayEquals(intArrayOf(1, 2, 3), parsed.data.getIntArray("ints"))
    }

    @Test
    fun rejectsInvalidInput() {
        assertThrows(IllegalArgumentException::class.java) {
            VendorData.deserialize("not an entry")
        }
        assertThrows(IllegalArgumentException::class.java) {
            VendorData.deserialize("000000:<bundle />")
        }
        assertThrows(IllegalArgumentException::class.java) {
            VendorData.deserialize("00aabb:<bundle>")
        }
    }
}
