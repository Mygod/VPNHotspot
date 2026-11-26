package be.mygod.vpnhotspot.net

import be.mygod.vpnhotspot.root.daemon.BridgeInterface
import be.mygod.vpnhotspot.root.daemon.LinkTopologySnapshot
import be.mygod.vpnhotspot.root.daemon.Neighbour
import be.mygod.vpnhotspot.root.daemon.NeighbourDelta
import be.mygod.vpnhotspot.root.daemon.NeighbourMonitorUpdate
import be.mygod.vpnhotspot.root.daemon.NeighbourState
import okio.ByteString.Companion.toByteString
import org.junit.Assert.assertEquals
import org.junit.Test

class NeighbourMonitorStateTest {
    private fun upsert(host: Int) = NeighbourDelta(delta = NeighbourDelta.Delta.Upsert(Neighbour(
        address = byteArrayOf(192.toByte(), 168.toByte(), 49, host.toByte()).toByteString(),
        dev = "p2p0",
        state = NeighbourState.NEIGHBOUR_STATE_VALID,
    )))

    private val NeighbourMonitorState.hosts get() = neighbours.keys.map { it.first.toByteArray()[3].toInt() }.toSet()

    @Test
    fun replacementDropsNeighboursAnUpdateWithoutItMerges() {
        val known = NeighbourMonitorState() + NeighbourMonitorUpdate(neighbour_deltas = listOf(upsert(2)))
        val topology = LinkTopologySnapshot(listOf(BridgeInterface("br0", listOf("p2p0"))))

        val merged = known + NeighbourMonitorUpdate(link_topology = topology)
        assertEquals(setOf(2), merged.hosts)
        assertEquals(mapOf("p2p0" to "br0"), merged.bridgeMasterByMember)

        assertEquals(emptySet<Int>(), (merged + NeighbourMonitorUpdate(replace_neighbours = true)).hosts)

        val replaced = merged + NeighbourMonitorUpdate(neighbour_deltas = listOf(upsert(3)), replace_neighbours = true)
        assertEquals(setOf(3), replaced.hosts)
        assertEquals(mapOf("p2p0" to "br0"), replaced.bridgeMasterByMember)
        assertEquals(setOf(3, 4), (replaced + NeighbourMonitorUpdate(neighbour_deltas = listOf(upsert(4)))).hosts)
    }
}
