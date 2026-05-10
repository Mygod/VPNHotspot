package be.mygod.vpnhotspot.client

import android.net.LinkAddress
import be.mygod.vpnhotspot.root.daemon.DaemonProto

data class ClientAddressInfo(var state: DaemonProto.NeighbourState = DaemonProto.NeighbourState.NEIGHBOUR_STATE_UNSET,
                             val address: LinkAddress? = null, val hostname: String? = null) {
    companion object {
        private val getDeprecationTime by lazy { LinkAddress::class.java.getDeclaredMethod("getDeprecationTime") }
        private val getExpirationTime by lazy { LinkAddress::class.java.getDeclaredMethod("getExpirationTime") }
    }
    val deprecationTime get() = getDeprecationTime(address) as Long
    val expirationTime get() = getExpirationTime(address) as Long
}
