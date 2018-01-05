package be.mygod.vpnhotspot

import be.mygod.vpnhotspot.App.Companion.app
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
                "while ip rule del priority 17999; do done")
    }

    class InterfaceNotFoundException : IOException() {
        override val message: String get() = app.getString(R.string.exception_interface_not_found)
    }

    val hostAddress = ownerAddress ?: NetworkInterface.getByName(downstream)?.inetAddresses?.asSequence()
            ?.singleOrNull { it is Inet4Address } ?: throw InterfaceNotFoundException()
    private val startScript = LinkedList<String>()
    private val stopScript = LinkedList<String>()

    fun ipForward(): Routing {
        startScript.add("echo 1 >/proc/sys/net/ipv4/ip_forward")
        return this
    }

    /**
     * Since Android 5.0, RULE_PRIORITY_TETHERING = 18000.
     * This also works for Wi-Fi direct where there's no rule at 18000.
     *
     * Source: https://android.googlesource.com/platform/system/netd/+/b9baf26/server/RouteController.cpp#65
     */
    fun rule(): Routing {
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
        val hostAddress = hostAddress.hostAddress
        startScript.add("iptables -t nat -A PREROUTING -i $downstream -p tcp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")
        startScript.add("iptables -t nat -A PREROUTING -i $downstream -p udp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")
        stopScript.addFirst("iptables -t nat -D PREROUTING -i $downstream -p tcp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")
        stopScript.addFirst("iptables -t nat -D PREROUTING -i $downstream -p udp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")
        return this
    }

    fun start() = noisySu(startScript)
    fun stop() = noisySu(stopScript)
}
