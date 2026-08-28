package be.mygod.vpnhotspot.shizuku

import be.mygod.vpnhotspot.root.daemon.ShizukuSessionConfig
import okio.ByteString.Companion.toByteString
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.net.InetAddress

/**
 * The whole published config, driven through the exact owner the session and its daemon connection call.
 *
 * Every case below asserts the built [ShizukuSessionConfig], not the counters behind it, because the config
 * is what the daemon refuses: it checks the generation against the upstream fields it retires, so a counter
 * that is right in a field but wrong in the message is a session that ends.
 *
 * The two calls each publication makes are the two production makes, in the same order and for the same
 * reason: the session builds a config on the observation's own lane, and the daemon's writer stamps the
 * sequence when one really goes out.
 */
class SessionPublicationTest {
    private companion object {
        private fun packed(vararg addresses: String) = addresses.map {
            InetAddress.getByName(it).address.toByteString()
        }

        /** Exactly what a session declares, so the packed bytes asserted below are the real ones. */
        val VIRTUAL = packed("192.0.2.2", "fd00::53")
        val GATEWAY = packed("192.0.2.1", "2001:db8:1::1")

        /** Two `Network` handles, which are opaque to everything here: only equality matters. */
        const val WIFI = 0x1234L
        const val VPN = 0x5678L
    }

    private val publication = SessionPublication(VIRTUAL, GATEWAY)

    private fun publish(admit: Boolean = true, network: Long? = WIFI, index: Int? = 7) =
        publication.stamping(publication.build(admit, network, index))

    /** The first config is already valid on both counters, whatever the upstream observation said. */
    @Test
    fun theFirstPublicationIsNonzeroOnEveryCounter() {
        assertEquals(0L, publication.sequence)
        assertEquals(1L, publication.upstreamGeneration)

        val first = publish(admit = false, network = null, index = null)
        assertEquals(1L, first.sequence)
        assertEquals(1L, first.upstream_generation)
        assertFalse(first.admit)
        assertNull("no selectable network is not a failure", first.upstream_network)
        assertNull(first.upstream_interface_index)
        assertEquals(VIRTUAL, first.virtual_addresses)
        assertEquals(GATEWAY, first.gateway_addresses)

        // And publishing the same observation again moves nothing but the sequence: it is not a change.
        val second = publish(admit = false, network = null, index = null)
        assertEquals(2L, second.sequence)
        assertEquals(1L, second.upstream_generation)
    }

    /** What a level-triggered resend of an unchanged observation costs: a sequence, and nothing else. */
    @Test
    fun anUnchangedUpstreamAdvancesOnlyTheSequence() {
        val first = publish()
        assertEquals(1L, first.sequence)
        repeat(8) { round ->
            val next = publish()
            assertEquals("the sequence is the only counter a resend moves", first.sequence + 1 + round,
                next.sequence)
            assertEquals(first.upstream_generation, next.upstream_generation)
            assertEquals(first.upstream_network, next.upstream_network)
            assertEquals(first.upstream_interface_index, next.upstream_interface_index)
        }
    }

    /**
     * Admission is the one field with no retirement behind it, in both directions and however often it moves.
     *
     * What that buys is narrow, and worth stating exactly: nothing is retired *for* the transition. It is not
     * a pause, and the daemon does not go on serving through one - it drops what it reads from the TUN while
     * admission is closed and refreshes no lifetime, so a flow can still expire inside the interval and what a
     * reopened session resumes with is whatever independently survived it.
     *
     * A generation moving here would instead retire, at once and for a transition nothing observed, every UDP
     * mapping, Echo session and ordinary TCP flow, and drop the output already queued under it. Reassembly
     * contexts and DNS-over-TCP transports would survive even that, holding no selected-network socket. The
     * daemon has no evidence that would justify any of it: Android's conntrack owns the mapping between a
     * TUN-visible tuple and a physical client, so neither side can tell that one changed hands.
     */
    @Test
    fun openingAndClosingAdmissionRetiresNothing() {
        val serving = publish()
        assertTrue(serving.admit)
        assertEquals(1L, serving.upstream_generation)

        // Three rounds of the transition, each with a level-triggered resend inside the closed interval: the
        // ordered stop's first step closes admission, and any observation after it republishes the same state.
        for (round in 0 until 3) {
            val closing = publish(admit = false)
            assertFalse(closing.admit)
            assertEquals("closing admission is not a retirement", serving.upstream_generation,
                closing.upstream_generation)
            assertEquals(serving.upstream_network, closing.upstream_network)
            assertEquals(serving.upstream_interface_index, closing.upstream_interface_index)
            // Losing it again while already closed, which is a resend rather than a change.
            val stillClosed = publish(admit = false)
            assertFalse(stillClosed.admit)
            assertEquals("a resend while closed is not a retirement either", serving.upstream_generation,
                stillClosed.upstream_generation)
            // And getting it back, which is just as free.
            val reopened = publish()
            assertTrue(reopened.admit)
            assertEquals("reopening admission is not a retirement either", serving.upstream_generation,
                reopened.upstream_generation)
            assertEquals(serving.sequence + 1 + 3 * round, closing.sequence)
            assertEquals(closing.sequence + 1, stillClosed.sequence)
            assertEquals(stillClosed.sequence + 1, reopened.sequence)
        }
        assertEquals(1L, publication.upstreamGeneration)
    }

    /**
     * An upstream change advances the generation and the sequence, and admission travels beside it untouched.
     *
     * A handover retires the sockets bound to the network that changed, and nothing else - so a config that
     * carries both a new handle and a closed admission still says exactly one thing about retirement.
     */
    @Test
    fun anUpstreamChangeAdvancesTheGenerationAndTheSequenceAlone() {
        val first = publish()

        // A handover: this app's default became a VPN, and the collector advances the generation before the
        // config carrying the new handle is built.
        assertEquals(2L, publication.advanceUpstream())
        val handover = publish(network = VPN)
        assertEquals(2L, handover.sequence)
        assertEquals(2L, handover.upstream_generation)
        assertEquals(VPN, handover.upstream_network)
        assertTrue(handover.admit)

        // The index moving on its own is the same fact, and needs the same retirement: it is resolved per
        // config, so it can move while the handle does not, and the sockets pinned behind it are just as
        // stale. Nothing advances the generation for it - the selection did not change, so no observation
        // fired - which is exactly why the owner has to notice the raw field itself.
        val reindexed = publish(admit = false, network = VPN, index = 9)
        assertEquals(3L, reindexed.sequence)
        assertEquals(3L, reindexed.upstream_generation)
        assertEquals(VPN, reindexed.upstream_network)
        assertEquals(9, reindexed.upstream_interface_index)
        assertFalse(reindexed.admit)
        assertEquals(first.virtual_addresses, reindexed.virtual_addresses)
        assertEquals(first.gateway_addresses, reindexed.gateway_addresses)
    }

    /**
     * The interface index arriving on its own advances the generation, with no observation behind it.
     *
     * This is the shape production really produces: the session advances the generation when the selected
     * `Upstream` *value* changes, but `if_nametoindex` is resolved while each config is built, so an index
     * that could not be resolved at selection time arrives on some later push. The daemon refuses a raw field
     * that moved with nothing retiring the sockets bound behind it, so the owner has to notice it.
     */
    @Test
    fun anInterfaceIndexOnlyChangeAdvancesTheGenerationOnItsOwn() {
        val unresolved = publish(network = WIFI, index = null)
        assertEquals(1L, unresolved.upstream_generation)
        assertEquals(WIFI, unresolved.upstream_network)
        assertNull(unresolved.upstream_interface_index)

        val resolved = publish(network = WIFI, index = 7)
        assertEquals("the index moved, so the generation had to", 2L, resolved.upstream_generation)
        assertEquals(7, resolved.upstream_interface_index)
        assertEquals(WIFI, resolved.upstream_network)
        assertEquals(2L, publication.upstreamGeneration)

        // And a resend of that same pair moves nothing.
        val resend = publish(network = WIFI, index = 7)
        assertEquals(2L, resend.upstream_generation)
        assertEquals(3L, resend.sequence)
    }

    /** The handle moving without an observation is the same fact, and needs the same axis. */
    @Test
    fun aNetworkOnlyChangeAdvancesTheGenerationOnItsOwn() {
        assertEquals(1L, publish().upstream_generation)

        val moved = publish(network = VPN)
        assertEquals(2L, moved.upstream_generation)
        assertEquals(VPN, moved.upstream_network)
        assertEquals(2L, publication.upstreamGeneration)
        assertEquals(2L, publish(network = VPN).upstream_generation)
    }

    /**
     * An observed selection change and the raw fields it moves are one logical change, so they advance the
     * generation once between them - whichever half the owner notices.
     */
    @Test
    fun anExplicitAdvanceCoalescesWithTheRawChangeItCaused() {
        assertEquals(1L, publish().upstream_generation)

        // The collector saw the selection change and advanced; the config built from it then carries both a
        // new handle and a new index.
        assertEquals(2L, publication.advanceUpstream())
        val handover = publish(network = VPN, index = 9)
        assertEquals("one logical change, one generation", 2L, handover.upstream_generation)
        assertEquals(VPN, handover.upstream_network)
        assertEquals(9, handover.upstream_interface_index)
        assertEquals(2L, publication.upstreamGeneration)
    }

    /**
     * A `LinkProperties` change on the same `Network` still advances the generation, and that advance
     * survives to the config it belongs to.
     *
     * The raw pair is equal across it, so nothing in the config itself says anything moved - which is the
     * whole reason the session advances it explicitly: a handle survives a `LinkProperties` change that
     * invalidates the state pinned behind it.
     */
    @Test
    fun anExplicitAdvanceWithEqualRawFieldsStillReachesTheNextConfig() {
        assertEquals(1L, publish().upstream_generation)

        assertEquals(2L, publication.advanceUpstream())
        val next = publish()
        assertEquals(2L, next.upstream_generation)
        assertEquals(WIFI, next.upstream_network)
        assertEquals(7, next.upstream_interface_index)
        assertEquals("nothing raw moved, so nothing more is owed", 2L, publish().upstream_generation)
    }

    /**
     * A raw-field candidate that is never stamped burns no generation, so its successor is one step away
     * rather than two.
     *
     * This is the coalescing the app really does: a config built while an earlier one is still awaiting its
     * acknowledgement sits in a single pending slot, and a later observation replaces it there. Committing at
     * build time would make the superseded candidate the state its successor is compared against, and one
     * logical change would advance the generation twice.
     */
    @Test
    fun aSupersededRawFieldCandidateCommitsNothing() {
        val sent = publish(network = WIFI, index = null)
        assertEquals(1L, sent.upstream_generation)

        val candidate = publication.build(true, WIFI, 7)
        assertEquals(2L, candidate.upstream_generation)
        assertEquals("a candidate is not a publication", 1L, publication.upstreamGeneration)
        assertEquals("nothing was written, so nothing was sequenced", 1L, publication.sequence)

        // Superseded by a config carrying that same raw change, which is still one step from what was sent.
        val successor = publish(network = WIFI, index = 7)
        assertEquals("one step, not two", 2L, successor.upstream_generation)
        assertEquals(2L, successor.sequence)
        assertEquals(7, successor.upstream_interface_index)
        assertEquals(2L, publication.upstreamGeneration)
    }

    /** One observation, and what the session's collectors do with it before a config is built. */
    private data class Observation(
        val what: String,
        val upstream: Boolean = false,
        val admit: Boolean = true,
        val network: Long? = WIFI,
        val index: Int? = 7,
    )

    /**
     * A session's whole run of observations, checked against the rules the daemon applies to a successor:
     * the sequence strictly increases, the generation never moves backwards, a raw upstream field never moves
     * without the generation that retires what is bound behind it, and neither address list moves either.
     */
    @Test
    fun successivePublicationsStayMonotonicAcrossAWholeSession() {
        var previous: ShizukuSessionConfig? = null
        for (observation in listOf(
            Observation("the first config", admit = false),
            Observation("tethering named this network, so it is serving"),
            Observation("a resend of the same everything"),
            Observation("tethering stopped naming this network", admit = false),
            Observation("a resend while unconfirmed", admit = false),
            Observation("a VPN came up while still unconfirmed", upstream = true, admit = false,
                network = VPN),
            Observation("its index resolved", upstream = true, admit = false, network = VPN, index = 9),
            Observation("tethering named it again", network = VPN, index = 9),
            Observation("nothing is selectable", upstream = true, admit = false, network = null,
                index = null),
        )) {
            if (observation.upstream) publication.advanceUpstream()
            val config = publication.stamping(
                publication.build(observation.admit, observation.network, observation.index))
            assertEquals("${observation.what}: admission is published as observed", observation.admit,
                config.admit)
            val last = previous
            if (last != null) {
                assertTrue("${observation.what}: the sequence must strictly increase",
                    config.sequence > last.sequence)
                assertTrue("${observation.what}: the generation went backwards",
                    config.upstream_generation >= last.upstream_generation)
                if (config.upstream_network != last.upstream_network ||
                    config.upstream_interface_index != last.upstream_interface_index) {
                    assertNotEquals("${observation.what}: the upstream moved with nothing retiring it",
                        last.upstream_generation, config.upstream_generation)
                } else if (config.admit != last.admit) {
                    assertEquals("${observation.what}: admission alone retired something",
                        last.upstream_generation, config.upstream_generation)
                }
                assertEquals(last.virtual_addresses, config.virtual_addresses)
                assertEquals(last.gateway_addresses, config.gateway_addresses)
            }
            previous = config
        }
        val last = checkNotNull(previous)
        // One sequence per publication and one generation per upstream observation, never more - and the four
        // admission changes in between bought nothing at all.
        assertEquals(9L, last.sequence)
        assertEquals(4L, last.upstream_generation)
    }
}
