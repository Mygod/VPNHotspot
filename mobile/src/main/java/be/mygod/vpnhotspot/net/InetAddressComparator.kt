package be.mygod.vpnhotspot.net

import java.net.InetAddress

object InetAddressComparator : Comparator<InetAddress> {
    override fun compare(o1: InetAddress?, o2: InetAddress?): Int {
        if (o1 == null && o2 == null) return 0
        val a1 = o1?.address
        val a2 = o2?.address
        val r = (a1?.size ?: 0).compareTo(a2?.size ?: 0)
        return if (r == 0) a1!!.zip(a2!!).map { (l, r) -> l - r }.find { it != 0 } ?: 0 else r
    }
}
