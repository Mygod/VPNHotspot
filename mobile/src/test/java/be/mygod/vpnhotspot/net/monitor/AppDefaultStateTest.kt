package be.mygod.vpnhotspot.net.monitor

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

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
    fun publicationWaitsForAllOrderedFacts() {
        assertNull(state.upstream)
        state.available(wifi)
        assertNull(state.upstream)
        assertTrue(state.properties(wifi, wifiProps))
        assertNull(state.upstream)
        assertTrue(state.blocked(wifi, false))
        assertEquals(wifi to wifiProps, state.upstream)
    }

    @Test
    fun handoverImmediatelyRetiresThePredecessor() {
        arrive(wifi, wifiProps)
        state.available(vpn)
        assertNull(state.upstream)
        state.properties(vpn, vpnProps)
        assertNull(state.upstream)
        state.blocked(vpn, false)
        assertEquals(vpn to vpnProps, state.upstream)
    }

    @Test
    fun blockedStatusFailsClosedAndCanRecover() {
        state.available(wifi)
        state.properties(wifi, wifiProps)
        state.blocked(wifi, true)
        assertNull(state.upstream)
        state.blocked(wifi, false)
        assertEquals(wifi to wifiProps, state.upstream)
        state.blocked(wifi, true)
        assertNull(state.upstream)
    }

    @Test
    fun staleCallbacksCannotChangeTheCurrentNetwork() {
        arrive(vpn, vpnProps)
        assertFalse(state.properties(wifi, wifiProps))
        assertFalse(state.blocked(wifi, false))
        assertFalse(state.lost(wifi))
        assertEquals(vpn to vpnProps, state.upstream)
    }

    @Test
    fun currentPropertyUpdatesRepublishAndLossClears() {
        arrive(wifi, wifiProps)
        val changed = Props("wlan0+")
        assertTrue(state.properties(wifi, changed))
        assertEquals(wifi to changed, state.upstream)
        assertTrue(state.lost(wifi))
        assertNull(state.upstream)
    }
}
