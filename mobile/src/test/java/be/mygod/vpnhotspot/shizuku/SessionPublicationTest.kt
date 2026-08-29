package be.mygod.vpnhotspot.shizuku

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class SessionPublicationTest {
    private companion object {
        const val WIFI = 0x1234L
        const val VPN = 0x5678L
    }

    private val publication = SessionPublication()

    private fun publish(admit: Boolean = true, network: Long? = WIFI, index: Int? = 7) =
        publication.publish(publication.build(admit, network, index))

    @Test
    fun admissionAndResendsDoNotRetireUpstreamState() {
        val unavailable = publish(admit = false, network = null, index = null)
        assertEquals(1L, unavailable.upstream_generation)
        assertFalse(unavailable.admit)
        assertNull(unavailable.upstream_network)
        assertNull(unavailable.upstream_interface_index)
        assertEquals(1L, publish(admit = false, network = null, index = null).upstream_generation)

        val available = publish()
        assertEquals(2L, available.upstream_generation)
        assertEquals(2L, publish(admit = false).upstream_generation)
        assertEquals(2L, publish(admit = false).upstream_generation)
        assertEquals(2L, publish().upstream_generation)
        assertTrue(publish().admit)
    }

    @Test
    fun eachRawUpstreamFieldRetiresWhenItMoves() {
        val unresolved = publish(index = null)
        assertEquals(1L, unresolved.upstream_generation)

        val resolved = publish(index = 7)
        assertEquals(2L, resolved.upstream_generation)
        assertEquals(7, resolved.upstream_interface_index)

        val moved = publish(network = VPN, index = 7)
        assertEquals(3L, moved.upstream_generation)
        assertEquals(VPN, moved.upstream_network)
        assertEquals(3L, publish(network = VPN, index = 7).upstream_generation)
    }

    @Test
    fun explicitAdvancesCoalesceWithRawChangesAndSurviveEqualFields() {
        assertEquals(1L, publish().upstream_generation)

        assertEquals(2L, publication.advanceUpstream())
        val moved = publish(network = VPN, index = 9)
        assertEquals(2L, moved.upstream_generation)
        assertEquals(VPN, moved.upstream_network)
        assertEquals(9, moved.upstream_interface_index)

        assertEquals(3L, publication.advanceUpstream())
        val equalRawFields = publish(network = VPN, index = 9)
        assertEquals(3L, equalRawFields.upstream_generation)
        assertEquals(3L, publish(network = VPN, index = 9).upstream_generation)
    }

    @Test
    fun supersededCandidateCommitsNothing() {
        assertEquals(1L, publish(index = null).upstream_generation)

        val candidate = publication.build(true, WIFI, 7)
        assertEquals(2L, candidate.upstream_generation)
        assertEquals(1L, publication.upstreamGeneration)

        val successor = publish(index = 7)
        assertEquals(2L, successor.upstream_generation)
        assertEquals(7, successor.upstream_interface_index)
        assertEquals(2L, publication.upstreamGeneration)
    }
}
