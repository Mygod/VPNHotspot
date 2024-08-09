package be.mygod.vpnhotspot.client

import android.net.LinkAddress
import be.mygod.vpnhotspot.net.IpNeighbour

data class ClientAddressInfo(var state: IpNeighbour.State = IpNeighbour.State.UNSET,
                             val address: LinkAddress? = null, val hostname: String? = null) {
    companion object {
        private val getDeprecationTime by lazy { LinkAddress::class.java.getDeclaredMethod("getDeprecationTime") }
        private val getExpirationTime by lazy { LinkAddress::class.java.getDeclaredMethod("getExpirationTime") }
    }
    val deprecationTime get() = getDeprecationTime(address) as Long
    val expirationTime get() = getExpirationTime(address) as Long
}
