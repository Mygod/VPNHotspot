package be.mygod.vpnhotspot

import java.io.IOException
import java.net.Inet4Address
import java.net.InetAddress
import java.net.NetworkInterface
import java.util.*

class Routing(private val upstream: String, val downstream: String, ownerAddress: InetAddress? = null) {
    companion object {
        fun clean() = noisySu(
                "iptables -t nat -F PREROUTING",
                "while iptables -D FORWARD -j vpnhotspot_fwd; do done",
                "iptables -F vpnhotspot_fwd",
                "iptables -X vpnhotspot_fwd",
                "while ip rule del lookup 62; do done",
                "ip route flush table 62",
                "while ip rule del priority 17999; do done")
    }

    class InterfaceNotFoundException : IOException()

    val hostAddress = NetworkInterface.getByName(downstream)?.interfaceAddresses
            ?.singleOrNull { if (ownerAddress == null) it.address is Inet4Address else it.address == ownerAddress }
            ?: throw InterfaceNotFoundException()
    private val startScript = LinkedList<String>()
    private val stopScript = LinkedList<String>()

    fun p2pRule(): Routing {
        val address = hostAddress.address.address
        val subnetPrefixLength = hostAddress.networkPrefixLength
        // clear suffix bits
        var done = subnetPrefixLength.toInt()
        while (done < address.size shl 3) {
            val index = done shr 3
            address[index] = (address[index].toInt() and (0x7f00 shr (done and 7))).toByte()
            done = (index + 1) shl 3
        }
        startScript.add("echo 1 >/proc/sys/net/ipv4/ip_forward")    // Wi-Fi direct doesn't enable ip_forward
        startScript.add("ip route add default dev $upstream scope link table 62")
        startScript.add("ip route add ${InetAddress.getByAddress(address).hostAddress}/$subnetPrefixLength dev $downstream scope link table 62")
        startScript.add("ip route add broadcast 255.255.255.255 dev $downstream scope link table 62")
        startScript.add("ip rule add iif $downstream lookup 62")
        // removing each rule may fail if downstream is already removed
        stopScript.addFirst("ip route flush table 62")
        stopScript.addFirst("ip rule del iif $downstream lookup 62")
        return this
    }

    /* Since Android 5.0, RULE_PRIORITY_TETHERING = 18000.
     * https://android.googlesource.com/platform/system/netd/+/b9baf26/server/RouteController.cpp#65 */
    fun apRule(): Routing {
        startScript.add("ip rule add from all iif $downstream lookup $upstream priority 17999")
        stopScript.addFirst("ip rule del from all iif $downstream lookup $upstream priority 17999")
        return this
    }

    fun forward(): Routing {
        startScript.add("iptables -N vpnhotspot_fwd")
        startScript.add("iptables -A vpnhotspot_fwd -i $upstream -o $downstream -m state --state ESTABLISHED,RELATED -j ACCEPT")
        startScript.add("iptables -A vpnhotspot_fwd -i $downstream -o $upstream -j ACCEPT")
        startScript.add("iptables -I FORWARD -j vpnhotspot_fwd")
        stopScript.addFirst("iptables -X vpnhotspot_fwd")
        stopScript.addFirst("iptables -F vpnhotspot_fwd")
        stopScript.addFirst("iptables -D FORWARD -j vpnhotspot_fwd")
        return this
    }

    fun dnsRedirect(dns: String): Routing {
        val hostAddress = hostAddress.address.hostAddress
        startScript.add("iptables -t nat -A PREROUTING -i $downstream -p tcp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")
        startScript.add("iptables -t nat -A PREROUTING -i $downstream -p udp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")
        stopScript.addFirst("iptables -t nat -D PREROUTING -i $downstream -p tcp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")
        stopScript.addFirst("iptables -t nat -D PREROUTING -i $downstream -p udp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")
        return this
    }

    fun start() = noisySu(startScript)
    fun stop() = noisySu(stopScript)
}
