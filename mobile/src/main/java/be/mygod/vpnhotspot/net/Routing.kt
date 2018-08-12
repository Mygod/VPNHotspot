package be.mygod.vpnhotspot.net

import android.os.Build
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.util.debugLog
import be.mygod.vpnhotspot.util.noisySu
import java.net.*
import java.util.*

class Routing(val upstream: String?, private val downstream: String, ownerAddress: InterfaceAddress? = null) {
    companion object {
        /**
         * -w <seconds> is not supported on 7.1-.
         * Fortunately there also isn't a time limit for starting a foreground service back in 7.1-.
         *
         * Source: https://android.googlesource.com/platform/external/iptables/+/android-5.0.0_r1/iptables/iptables.c#1574
         */
        private val IPTABLES = if (Build.VERSION.SDK_INT >= 26) "iptables -w 1" else "iptables -w"

        fun clean() = noisySu(
                "$IPTABLES -t nat -F PREROUTING",
                "quiet while $IPTABLES -D FORWARD -j vpnhotspot_fwd; do done",
                "$IPTABLES -F vpnhotspot_fwd",
                "$IPTABLES -X vpnhotspot_fwd",
                "quiet while $IPTABLES -t nat -D POSTROUTING -j vpnhotspot_masquerade; do done",
                "$IPTABLES -t nat -F vpnhotspot_masquerade",
                "$IPTABLES -t nat -X vpnhotspot_masquerade",
                "quiet while ip rule del priority 17900; do done",
                "quiet while ip rule del iif lo uidrange 0-0 lookup local_network priority 11000; do done",
                report = false)
    }

    class InterfaceNotFoundException : SocketException() {
        override val message: String get() = app.getString(R.string.exception_interface_not_found)
    }

    val hostAddress = ownerAddress ?: NetworkInterface.getByName(downstream)?.interfaceAddresses?.asSequence()
            ?.singleOrNull { it.address is Inet4Address } ?: throw InterfaceNotFoundException()
    private val startScript = LinkedList<String>()
    private val stopScript = LinkedList<String>()
    var started = false

    fun ipForward() {
        startScript.add("echo 1 >/proc/sys/net/ipv4/ip_forward")
    }

    fun disableIpv6() {
        startScript.add("echo 1 >/proc/sys/net/ipv6/conf/$downstream/disable_ipv6")
        stopScript.add("echo 0 >/proc/sys/net/ipv6/conf/$downstream/disable_ipv6")
    }

    /**
     * Since Android 5.0, RULE_PRIORITY_TETHERING = 18000.
     * This also works for Wi-Fi direct where there's no rule at 18000.
     *
     * Source: https://android.googlesource.com/platform/system/netd/+/b9baf26/server/RouteController.cpp#65
     */
    fun rule() {
        if (upstream != null) {
            startScript.add("ip rule add from all iif $downstream lookup $upstream priority 17900")
            // by the time stopScript is called, table entry for upstream may already get removed
            stopScript.addFirst("ip rule del from all iif $downstream priority 17900")
        }
    }

    fun forward(strict: Boolean = true) {
        startScript.add("quiet $IPTABLES -N vpnhotspot_fwd 2>/dev/null")
        if (strict) {
            check(upstream != null)
            startScript.add("$IPTABLES -A vpnhotspot_fwd -i $upstream -o $downstream -m state --state ESTABLISHED,RELATED -j ACCEPT")
            startScript.add("$IPTABLES -A vpnhotspot_fwd -i $downstream -o $upstream -j ACCEPT")
            stopScript.addFirst("$IPTABLES -D vpnhotspot_fwd -i $upstream -o $downstream -m state --state ESTABLISHED,RELATED -j ACCEPT")
            stopScript.addFirst("$IPTABLES -D vpnhotspot_fwd -i $downstream -o $upstream -j ACCEPT")
        } else {
            // for not strict mode, allow downstream packets to be redirected to anywhere
            // because we don't wanna keep track of default network changes
            startScript.add("$IPTABLES -A vpnhotspot_fwd -o $downstream -m state --state ESTABLISHED,RELATED -j ACCEPT")
            startScript.add("$IPTABLES -A vpnhotspot_fwd -i $downstream -j ACCEPT")
            stopScript.addFirst("$IPTABLES -D vpnhotspot_fwd -o $downstream -m state --state ESTABLISHED,RELATED -j ACCEPT")
            stopScript.addFirst("$IPTABLES -D vpnhotspot_fwd -i $downstream -j ACCEPT")
        }
        startScript.add("$IPTABLES -I FORWARD -j vpnhotspot_fwd")
        stopScript.addFirst("$IPTABLES -D FORWARD -j vpnhotspot_fwd")
    }

    fun masquerade(strict: Boolean = true) {
        val hostSubnet = "${hostAddress.address.hostAddress}/${hostAddress.networkPrefixLength}"
        startScript.add("quiet $IPTABLES -t nat -N vpnhotspot_masquerade 2>/dev/null")
        // note: specifying -i wouldn't work for POSTROUTING
        if (strict) {
            check(upstream != null)
            startScript.add("$IPTABLES -t nat -A vpnhotspot_masquerade -s $hostSubnet -o $upstream -j MASQUERADE")
            stopScript.addFirst("$IPTABLES -t nat -D vpnhotspot_masquerade -s $hostSubnet -o $upstream -j MASQUERADE")
        } else {
            startScript.add("$IPTABLES -t nat -A vpnhotspot_masquerade -s $hostSubnet -j MASQUERADE")
            stopScript.addFirst("$IPTABLES -t nat -D vpnhotspot_masquerade -s $hostSubnet -j MASQUERADE")
        }
        startScript.add("$IPTABLES -t nat -I POSTROUTING -j vpnhotspot_masquerade")
        stopScript.addFirst("$IPTABLES -t nat -D POSTROUTING -j vpnhotspot_masquerade")
    }

    fun dnsRedirect(dnses: List<InetAddress>) {
        val hostAddress = hostAddress.address.hostAddress
        val dns = dnses.firstOrNull { it is Inet4Address }?.hostAddress ?: app.pref.getString("service.dns", "8.8.8.8")
        debugLog("Routing", "Using $dns from ($dnses)")
        startScript.add("$IPTABLES -t nat -A PREROUTING -i $downstream -p tcp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")
        startScript.add("$IPTABLES -t nat -A PREROUTING -i $downstream -p udp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")
        stopScript.addFirst("$IPTABLES -t nat -D PREROUTING -i $downstream -p tcp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")
        stopScript.addFirst("$IPTABLES -t nat -D PREROUTING -i $downstream -p udp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")
    }

    /**
     * Similarly, assuming RULE_PRIORITY_VPN_OUTPUT_TO_LOCAL = 11000.
     * Normally this is used to forward packets from remote to local, but it works anyways. It just needs to be before
     * RULE_PRIORITY_SECURE_VPN = 12000. It would be great if we can gain better understanding into why this is only
     * needed on some of the devices but not others.
     *
     * Source: https://android.googlesource.com/platform/system/netd/+/b9baf26/server/RouteController.cpp#57
     */
    fun dhcpWorkaround() {
        startScript.add("ip rule add iif lo uidrange 0-0 lookup local_network priority 11000")
        stopScript.addFirst("ip rule del iif lo uidrange 0-0 lookup local_network priority 11000")
    }

    fun start(): Boolean {
        if (started) return true
        started = true
        if (noisySu(startScript) != true) stop()
        return started
    }
    fun stop(): Boolean {
        if (!started) return true
        started = false
        return noisySu(stopScript, false) == true
    }
}
