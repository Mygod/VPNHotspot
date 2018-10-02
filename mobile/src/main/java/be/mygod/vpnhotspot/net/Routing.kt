package be.mygod.vpnhotspot.net

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.os.Build
import android.os.IBinder
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.client.Client
import be.mygod.vpnhotspot.client.ClientMonitorService
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.util.computeIfAbsentCompat
import be.mygod.vpnhotspot.util.debugLog
import be.mygod.vpnhotspot.util.stopAndUnbind
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.crashlytics.android.Crashlytics
import java.net.*

/**
 * A transaction wrapper that helps set up routing environment.
 *
 * Once revert is called, this object no longer serves any purpose.
 */
class Routing(private val owner: Context, val upstream: String?, private val downstream: String,
              ownerAddress: InterfaceAddress? = null, private val strict: Boolean = true) : ServiceConnection {
    companion object {
        /**
         * -w <seconds> is not supported on 7.1-.
         * Fortunately there also isn't a time limit for starting a foreground service back in 7.1-.
         *
         * Source: https://android.googlesource.com/platform/external/iptables/+/android-5.0.0_r1/iptables/iptables.c#1574
         */
        private val IPTABLES = if (Build.VERSION.SDK_INT >= 26) "iptables -w 1" else "iptables -w"

        fun clean() {
            TrafficRecorder.clean()
            RootSession.use {
                it.submit("$IPTABLES -t nat -F PREROUTING")
                it.submit("while $IPTABLES -D FORWARD -j vpnhotspot_fwd; do done")
                it.submit("$IPTABLES -F vpnhotspot_fwd")
                it.submit("$IPTABLES -X vpnhotspot_fwd")
                it.submit("while $IPTABLES -t nat -D POSTROUTING -j vpnhotspot_masquerade; do done")
                it.submit("$IPTABLES -t nat -F vpnhotspot_masquerade")
                it.submit("$IPTABLES -t nat -X vpnhotspot_masquerade")
                it.submit("while ip rule del priority 17900; do done")
                it.submit("while ip rule del iif lo uidrange 0-0 lookup local_network priority 11000; do done")
            }
        }

        fun RootSession.Transaction.iptablesAdd(content: String, table: String = "filter") =
                exec("$IPTABLES -t $table -A $content", "$IPTABLES -t $table -D $content")
        fun RootSession.Transaction.iptablesInsert(content: String, table: String = "filter") =
                exec("$IPTABLES -t $table -I $content", "$IPTABLES -t $table -D $content")
    }

    class InterfaceNotFoundException : SocketException() {
        override val message: String get() = app.getString(R.string.exception_interface_not_found)
    }

    val hostAddress = ownerAddress ?: NetworkInterface.getByName(downstream)?.interfaceAddresses?.asSequence()
            ?.singleOrNull { it.address is Inet4Address } ?: throw InterfaceNotFoundException()
    private val transaction = RootSession.beginTransaction()
    private val subroutes = HashMap<InetAddress, Subroute>()
    private var clients: ClientMonitorService.Binder? = null

    fun ipForward() = transaction.exec("echo 1 >/proc/sys/net/ipv4/ip_forward")

    fun disableIpv6() = transaction.exec("echo 1 >/proc/sys/net/ipv6/conf/$downstream/disable_ipv6",
            "echo 0 >/proc/sys/net/ipv6/conf/$downstream/disable_ipv6")

    /**
     * Since Android 5.0, RULE_PRIORITY_TETHERING = 18000.
     * This also works for Wi-Fi direct where there's no rule at 18000.
     *
     * Source: https://android.googlesource.com/platform/system/netd/+/b9baf26/server/RouteController.cpp#65
     */
    fun rule() {
        if (upstream != null) {
            transaction.exec("ip rule add from all iif $downstream lookup $upstream priority 17900",
                    // by the time stopScript is called, table entry for upstream may already get removed
                    "ip rule del from all iif $downstream priority 17900")
        }
    }

    fun forward() {
        transaction.execQuiet("$IPTABLES -N vpnhotspot_fwd")
        transaction.iptablesInsert("FORWARD -j vpnhotspot_fwd")
        transaction.iptablesAdd("vpnhotspot_fwd -i $downstream ! -o $downstream -j DROP")   // ensure blocking works
        // the real forwarding filters will be added in Subroute when clients are connected
    }

    fun masquerade() {
        val hostSubnet = "${hostAddress.address.hostAddress}/${hostAddress.networkPrefixLength}"
        transaction.execQuiet("$IPTABLES -t nat -N vpnhotspot_masquerade")
        transaction.iptablesInsert("POSTROUTING -j vpnhotspot_masquerade", "nat")
        // note: specifying -i wouldn't work for POSTROUTING
        if (strict) {
            if (upstream != null) {
                transaction.iptablesAdd("vpnhotspot_masquerade -s $hostSubnet -o $upstream -j MASQUERADE", "nat")
            }   // else nothing needs to be done
        } else {
            transaction.iptablesAdd("vpnhotspot_masquerade -s $hostSubnet -j MASQUERADE", "nat")
        }
    }

    fun dnsRedirect(dnses: List<InetAddress>) {
        val hostAddress = hostAddress.address.hostAddress
        val dns = dnses.firstOrNull { it is Inet4Address }?.hostAddress ?: app.pref.getString("service.dns", "8.8.8.8")
        debugLog("Routing", "Using $dns from ($dnses)")
        transaction.iptablesAdd("PREROUTING -i $downstream -p tcp -d $hostAddress --dport 53 -j DNAT --to-destination $dns", "nat")
        transaction.iptablesAdd("PREROUTING -i $downstream -p udp -d $hostAddress --dport 53 -j DNAT --to-destination $dns", "nat")
    }

    /**
     * Similarly, assuming RULE_PRIORITY_VPN_OUTPUT_TO_LOCAL = 11000.
     * Normally this is used to forward packets from remote to local, but it works anyways. It just needs to be before
     * RULE_PRIORITY_SECURE_VPN = 12000. It would be great if we can gain better understanding into why this is only
     * needed on some of the devices but not others.
     *
     * Source: https://android.googlesource.com/platform/system/netd/+/b9baf26/server/RouteController.cpp#57
     */
    fun dhcpWorkaround() = transaction.exec("ip rule add iif lo uidrange 0-0 lookup local_network priority 11000",
            "ip rule del iif lo uidrange 0-0 lookup local_network priority 11000")

    fun commit() {
        transaction.commit()
        owner.bindService(Intent(owner, ClientMonitorService::class.java), this, Context.BIND_AUTO_CREATE)
    }
    fun revert() {
        stop()
        synchronized(subroutes) { subroutes.forEach { (_, subroute) -> subroute.close() } }
        transaction.revert()
    }

    /**
     * Only unregister client listener. This should only be used when a clean has just performed.
     */
    fun stop() = owner.stopAndUnbind(this)

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        clients = service as ClientMonitorService.Binder
        service.clientsChanged[this] = {
            synchronized(subroutes) {
                val toRemove = HashSet(subroutes.keys)
                for (client in it) if (!client.record.blocked) updateForClient(client, toRemove)
                for (address in toRemove) subroutes.remove(address)!!.close()
            }
        }
    }
    override fun onServiceDisconnected(name: ComponentName?) {
        val clients = clients ?: return
        clients.clientsChanged -= this
        this.clients = null
    }

    private fun updateForClient(client: Client, toRemove: HashSet<InetAddress>? = null) {
        for ((ip, _) in client.ip) if (ip is Inet4Address) {
            toRemove?.remove(ip)
            try {
                subroutes.computeIfAbsentCompat(ip) { Subroute(ip, client) }
            } catch (e: Exception) {
                Crashlytics.logException(e)
                e.printStackTrace()
                SmartSnackbar.make(e.localizedMessage).show()
            }
        }
    }

    private inner class Subroute(private val ip: Inet4Address, client: Client) : AutoCloseable {
        private val transaction = RootSession.beginTransaction().apply {
            try {
                val address by lazy { ip.hostAddress }
                if (!strict) {
                    // otw allow downstream packets to be redirected to anywhere
                    // because we don't wanna keep track of default network changes
                    iptablesInsert("vpnhotspot_fwd -i $downstream -s $address -j ACCEPT")
                    iptablesInsert("vpnhotspot_fwd -o $downstream -d $address -m state --state ESTABLISHED,RELATED -j ACCEPT")
                } else if (upstream != null) {
                    iptablesInsert("vpnhotspot_fwd -i $downstream -s $address -o $upstream -j ACCEPT")
                    iptablesInsert("vpnhotspot_fwd -i $upstream -o $downstream -d $address -m state --state ESTABLISHED,RELATED -j ACCEPT")
                }   // else nothing needs to be done
                commit()
            } catch (e: Exception) {
                revert()
                throw e
            }
        }

        init {
            TrafficRecorder.register(ip, if (strict) upstream else null, downstream, client.mac)
        }

        override fun close() {
            TrafficRecorder.unregister(ip, downstream)
            transaction.revert()
        }
    }
}
