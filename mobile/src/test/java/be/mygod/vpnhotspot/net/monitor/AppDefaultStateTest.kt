package be.mygod.vpnhotspot.net.monitor

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The rules [AppDefaultState] exists to keep, exercised in the callback order the framework guarantees.
 *
 * Stand-ins rather than `Network`/`LinkProperties`, which is the point of the extraction: the decision is
 * about which ordered facts have arrived, so it needs no Android runtime to state or to check.
 */
class AppDefaultStateTest {
    private data class Net(val id: Int)
    private data class Props(val name: String)

    private val state = AppDefaultState<Net, Props>()
    private val wifi = Net(1)
    private val vpn = Net(2)
    private val wifiProps = Props("wlan0")
    private val vpnProps = Props("tun0")

    private fun arrive(network: Net, properties: Props) {
        state.available(network)
        state.properties(network, properties)
        state.blocked(network, false)
    }

    @Test
    fun nothingBeforeAnyCallback() = assertNull(state.upstream)

    @Test
    fun availabilityAloneIsNotAnUpstream() {
        state.available(wifi)
        assertNull(state.upstream)
    }

    @Test
    fun propertiesWithoutBlockedStatusIsNotAnUpstream() {
        state.available(wifi)
        assertTrue(state.properties(wifi, wifiProps))
        assertNull(state.upstream)
    }

    @Test
    fun completeArrivalPublishes() {
        arrive(wifi, wifiProps)
        assertEquals(wifi to wifiProps, state.upstream)
    }

    /** The regression this class was extracted for: a handover must retire the old value at once. */
    @Test
    fun handoverRetiresThePredecessorBeforeItsSuccessorIsDescribed() {
        arrive(wifi, wifiProps)
        state.available(vpn)
        assertNull(state.upstream)
        assertTrue(state.properties(vpn, vpnProps))
        assertNull(state.upstream)
        assertTrue(state.blocked(vpn, false))
        assertEquals(vpn to vpnProps, state.upstream)
    }

    @Test
    fun blockedFailsClosed() {
        state.available(wifi)
        state.properties(wifi, wifiProps)
        assertTrue(state.blocked(wifi, true))
        assertNull(state.upstream)
    }

    @Test
    fun becomingBlockedRetractsALivePublication() {
        arrive(wifi, wifiProps)
        assertTrue(state.blocked(wifi, true))
        assertNull(state.upstream)
        assertTrue(state.blocked(wifi, false))
        assertEquals(wifi to wifiProps, state.upstream)
    }

    @Test
    fun lossFailsClosed() {
        arrive(wifi, wifiProps)
        assertTrue(state.lost(wifi))
        assertNull(state.upstream)
    }

    /** Late callbacks about a network that is no longer current change nothing and emit nothing. */
    @Test
    fun stalePropertiesAreIgnored() {
        arrive(wifi, wifiProps)
        state.available(vpn)
        assertFalse(state.properties(wifi, wifiProps))
        assertFalse(state.blocked(wifi, false))
        assertNull(state.upstream)
    }

    @Test
    fun staleLossDoesNotRetireTheCurrentNetwork() {
        arrive(wifi, wifiProps)
        state.available(vpn)
        state.properties(vpn, vpnProps)
        state.blocked(vpn, false)
        assertFalse(state.lost(wifi))
        assertEquals(vpn to vpnProps, state.upstream)
    }

    /** A property update on the live network republishes rather than retiring it. */
    @Test
    fun propertyUpdateOnTheCurrentNetworkRepublishes() {
        arrive(wifi, wifiProps)
        val renumbered = Props("wlan0-renumbered")
        assertTrue(state.properties(wifi, renumbered))
        assertEquals(wifi to renumbered, state.upstream)
    }
}
