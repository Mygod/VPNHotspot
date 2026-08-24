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
 * is what the daemon refuses: it checks the three axes against each other and against the fields they retire,
 * so an axis that is right in a field but wrong in the message is a session that ends.
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

        /** The TestNetwork's own contract, which is what a session is constructed with. */
        const val MTU = 1500

        /** Two `Network` handles, which are opaque to everything here: only equality matters. */
        const val WIFI = 0x1234L
        const val VPN = 0x5678L
    }

    private val publication = SessionPublication(VIRTUAL, GATEWAY, MTU)

    private fun publish(admit: Boolean = true, network: Long? = WIFI, index: Int? = 7) =
        publication.stamping(publication.build(admit, network, index))

    /** The first config is already valid on all three axes, whatever the upstream observation said. */
    @Test
    fun theFirstPublicationIsNonzeroOnEveryAxis() {
        assertEquals(0L, publication.sequence)
        assertEquals(1L, publication.upstreamGeneration)
        assertEquals(1L, publication.downstreamEpoch)

        val first = publish(admit = false, network = null, index = null)
        assertEquals(1L, first.sequence)
        assertEquals(1L, first.upstream_generation)
        assertEquals(1L, first.downstream_epoch)
        assertEquals(MTU, first.downstream_mtu_floor)
        assertFalse(first.admit)
        assertNull("no selectable network is not a failure", first.upstream_network)
        assertNull(first.upstream_interface_index)
        assertEquals(VIRTUAL, first.virtual_addresses)
        assertEquals(GATEWAY, first.gateway_addresses)

        // And publishing the same observation again moves neither axis: it is not a change.
        val second = publish(admit = false, network = null, index = null)
        assertEquals(2L, second.sequence)
        assertEquals(1L, second.upstream_generation)
        assertEquals(1L, second.downstream_epoch)
        assertEquals(MTU, second.downstream_mtu_floor)
    }

    /**
     * The MTU is the session's own fixed contract, so no publication can ever move it.
     *
     * This is the rule that makes the global path safe to leave the epoch alone for: the daemon refuses a
     * floor that moved without the epoch that retires the output already sized against it, and the only way
     * to guarantee that is for the floor never to be a measurement in the first place.
     */
    @Test
    fun everyConfigCarriesTheSameFixedMtu() {
        val configs = listOf(publish(), publish(network = VPN), publish(admit = false, network = null))
        publication.advanceDownstream()
        for (config in configs + publish()) assertEquals(MTU, config.downstream_mtu_floor)
    }

    /** What a level-triggered resend of an unchanged observation costs: a sequence, and nothing else. */
    @Test
    fun anUnchangedUpstreamAdvancesOnlyTheSequence() {
        val first = publish()
        assertEquals(1L, first.sequence)
        repeat(8) { round ->
            val next = publish()
            assertEquals("the sequence is the only axis a resend moves", first.sequence + 1 + round,
                next.sequence)
            assertEquals(first.upstream_generation, next.upstream_generation)
            assertEquals(first.downstream_epoch, next.downstream_epoch)
            assertEquals(first.upstream_network, next.upstream_network)
            assertEquals(first.upstream_interface_index, next.upstream_interface_index)
        }
        // Closing admission is the ordered stop's first step, and it retires nothing at all.
        val closing = publish(admit = false)
        assertFalse(closing.admit)
        assertEquals(first.upstream_generation, closing.upstream_generation)
        assertEquals(first.downstream_epoch, closing.downstream_epoch)
    }

    /**
     * An upstream-only change advances the generation and the sequence and leaves the epoch alone.
     *
     * The two axes are independent by design: a handover retires the sockets bound to the network that
     * changed, and nothing else. Advancing the epoch with it would retire every TUN-visible tuple - every
     * client's live flows - for a change no client saw.
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
        assertEquals(first.downstream_epoch, handover.downstream_epoch)

        // The index moving on its own is the same fact, and needs the same axis: it is resolved per config,
        // so it can move while the handle does not, and the sockets pinned behind it are just as stale.
        // Nothing advances the generation for it - the selection did not change, so no observation fired -
        // which is exactly why the owner has to notice the raw field itself.
        val reindexed = publish(network = VPN, index = 9)
        assertEquals(3L, reindexed.sequence)
        assertEquals(3L, reindexed.upstream_generation)
        assertEquals(VPN, reindexed.upstream_network)
        assertEquals(9, reindexed.upstream_interface_index)
        assertEquals(first.downstream_epoch, reindexed.downstream_epoch)
    }

    /**
     * Losing positive confirmation retires the downstream state, and does so once per loss.
     *
     * This is the only thing that moves the epoch now that the path is global: the session never asks which
     * interfaces Android is serving, so what breaks the correspondence between a TUN-visible tuple and a
     * client is tethering no longer naming the exact network this session owns - it may have rebuilt its NAT
     * behind an unchanged `Network` handle.
     */
    @Test
    fun aLossOfConfirmationAdvancesTheEpochAndNothingElse() {
        val first = publish()
        assertEquals(1L, first.downstream_epoch)

        publication.advanceDownstream()
        val retired = publish()
        assertEquals(2L, retired.downstream_epoch)
        assertEquals(2L, retired.sequence)
        assertEquals("a downstream transition is not an upstream one", 1L, retired.upstream_generation)
        assertEquals(MTU, retired.downstream_mtu_floor)

        // Every later level-triggered resend carries that same epoch, and retires nothing.
        repeat(3) { assertEquals(2L, publish().downstream_epoch) }
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
        assertEquals("the epoch retires downstreams, not upstreams", 1L, resolved.downstream_epoch)

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
     * logical change would advance the axis twice.
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
        val downstream: Boolean = false,
        val network: Long? = WIFI,
        val index: Int? = 7,
    )

    /**
     * A session's whole run of observations, checked against the rules the daemon applies to a successor:
     * the sequence strictly increases, neither axis moves backwards, a raw upstream field never moves without
     * the generation that retires what is bound behind it, the floor never moves at all, and neither address
     * list moves either.
     */
    @Test
    fun successivePublicationsAreMonotonicOnAllThreeAxes() {
        var previous: ShizukuSessionConfig? = null
        for (observation in listOf(
            Observation("the first config"),
            Observation("a resend of the same everything"),
            Observation("tethering stopped naming this network", downstream = true),
            Observation("a resend while unconfirmed"),
            Observation("a VPN came up", upstream = true, network = VPN),
            Observation("its index resolved", upstream = true, network = VPN, index = 9),
            Observation("tethering named it again, then lost it", downstream = true, network = VPN, index = 9),
            Observation("nothing is selectable", upstream = true, network = null, index = null),
        )) {
            if (observation.upstream) publication.advanceUpstream()
            if (observation.downstream) publication.advanceDownstream()
            val config = publication.stamping(
                publication.build(true, observation.network, observation.index))
            val last = previous
            if (last != null) {
                assertTrue("${observation.what}: the sequence must strictly increase",
                    config.sequence > last.sequence)
                assertTrue("${observation.what}: the generation went backwards",
                    config.upstream_generation >= last.upstream_generation)
                assertTrue("${observation.what}: the epoch went backwards",
                    config.downstream_epoch >= last.downstream_epoch)
                if (config.upstream_network != last.upstream_network ||
                    config.upstream_interface_index != last.upstream_interface_index) {
                    assertNotEquals("${observation.what}: the upstream moved with nothing retiring it",
                        last.upstream_generation, config.upstream_generation)
                }
                assertEquals("${observation.what}: the fixed MTU moved",
                    last.downstream_mtu_floor, config.downstream_mtu_floor)
                assertEquals(last.virtual_addresses, config.virtual_addresses)
                assertEquals(last.gateway_addresses, config.gateway_addresses)
            }
            previous = config
        }
        val last = checkNotNull(previous)
        // One sequence per publication, one generation per upstream observation, and one epoch per lost
        // confirmation - never more.
        assertEquals(8L, last.sequence)
        assertEquals(4L, last.upstream_generation)
        assertEquals(3L, last.downstream_epoch)
    }
}
