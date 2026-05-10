package be.mygod.vpnhotspot.root.daemon

import android.net.IpPrefix
import android.net.MacAddress
import android.net.Network
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.root.daemon.proto.DaemonProto
import com.google.protobuf.ByteString
import java.io.IOException
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress

object DaemonProtocol {
    data class SessionConfig(
        val downstream: String,
        val ipForward: Boolean,
        val masquerade: Routing.MasqueradeMode,
        val ipv6Block: Boolean,
        val primaryNetwork: Network?,
        val primaryRoutes: List<IpPrefix>,
        val fallbackNetwork: Network?,
        val primaryUpstreamInterfaces: List<String>,
        val fallbackUpstreamInterfaces: List<String>,
        val clients: List<ClientConfig>,
        val ipv6Nat: Ipv6NatConfig?,
    )

    data class ClientConfig(
        val mac: MacAddress,
        val ipv4: List<Inet4Address>,
    )

    data class Ipv6NatConfig(
        val prefixSeed: String,
    )

    sealed class NeighbourDelta {
        data class Upsert(val neighbour: NetlinkNeighbour) : NeighbourDelta()
        data class Delete(val ip: InetAddress, val dev: String) : NeighbourDelta()
    }

    class Command internal constructor(
        private val build: DaemonProto.ClientEnvelope.Builder.() -> Unit,
        private val description: String,
    ) {
        fun packet(callId: Long): ByteArray {
            require(callId > 0) { "Invalid daemon call id $callId" }
            return DaemonProto.ClientEnvelope.newBuilder().apply {
                setCallId(callId)
                build()
            }.build().toByteArray()
        }

        override fun toString() = description
    }

    fun cancel() = command("Cancel") {
        cancel = DaemonProto.CancelCommand.getDefaultInstance()
    }
    fun startSession(config: SessionConfig) = command("StartSession(config=$config)") {
        startSession = DaemonProto.StartSessionCommand.newBuilder().setConfig(config.toProto()).build()
    }
    fun replaceSession(sessionId: Long, config: SessionConfig) =
            command("ReplaceSession(sessionId=$sessionId, config=$config)") {
        replaceSession = DaemonProto.ReplaceSessionCommand.newBuilder()
            .setSessionId(sessionId)
            .setConfig(config.toProto())
            .build()
    }
    fun readTrafficCounters() = command("ReadTrafficCounters") {
        readTrafficCounters = DaemonProto.ReadTrafficCountersCommand.getDefaultInstance()
    }
    fun startNeighbourMonitor() = command("StartNeighbourMonitor") {
        startNeighbourMonitor = DaemonProto.StartNeighbourMonitorCommand.getDefaultInstance()
    }
    fun replaceStaticAddresses(dev: String, addresses: List<Pair<InetAddress, Int>>) =
            command("ReplaceStaticAddresses(dev=$dev, addresses=$addresses)") {
        replaceStaticAddresses = DaemonProto.ReplaceStaticAddressesCommand.newBuilder()
            .setInterface(dev)
            .addAllAddresses(addresses.map { (address, prefixLength) ->
                DaemonProto.IpAddressEntry.newBuilder()
                    .setAddress(address.toByteString())
                    .setPrefixLength(prefixLength)
                    .build()
            })
            .build()
    }
    fun deleteStaticAddresses(dev: String) = command("DeleteStaticAddresses(dev=$dev)") {
        deleteStaticAddresses = DaemonProto.DeleteStaticAddressesCommand.newBuilder().setInterface(dev).build()
    }
    fun cleanRouting(ipv6NatPrefixSeed: String) = command("CleanRouting(ipv6NatPrefixSeed=$ipv6NatPrefixSeed)") {
        cleanRouting = DaemonProto.CleanRoutingCommand.newBuilder().setIpv6NatPrefixSeed(ipv6NatPrefixSeed).build()
    }

    fun readAck(payload: DaemonTransport.ReplyPayload) {
        if (payload !is DaemonTransport.ReplyPayload.Ack) throw IOException("Unexpected daemon reply $payload")
    }

    fun readAck(payload: DaemonTransport.EventPayload) {
        if (payload !is DaemonTransport.EventPayload.Ack) throw IOException("Unexpected daemon event $payload")
    }

    fun readTrafficCounterLines(payload: DaemonTransport.ReplyPayload): List<String> {
        if (payload !is DaemonTransport.ReplyPayload.TrafficCounterLines) {
            throw IOException("Unexpected daemon reply $payload")
        }
        return payload.lines
    }

    fun readNeighbourDeltas(payload: DaemonTransport.EventPayload): List<NeighbourDelta> {
        if (payload !is DaemonTransport.EventPayload.NeighbourDeltas) {
            throw IOException("Unexpected daemon event $payload")
        }
        return payload.deltas.deltasList.map { delta ->
            when (delta.deltaCase) {
                DaemonProto.NeighbourDelta.DeltaCase.UPSERT -> {
                    val neighbour = delta.upsert
                    val state = when (neighbour.state) {
                        DaemonProto.NeighbourState.NEIGHBOUR_STATE_UNSET -> NetlinkNeighbour.State.UNSET
                        DaemonProto.NeighbourState.NEIGHBOUR_STATE_INCOMPLETE -> NetlinkNeighbour.State.INCOMPLETE
                        DaemonProto.NeighbourState.NEIGHBOUR_STATE_VALID -> NetlinkNeighbour.State.VALID
                        DaemonProto.NeighbourState.NEIGHBOUR_STATE_FAILED -> NetlinkNeighbour.State.FAILED
                        DaemonProto.NeighbourState.UNRECOGNIZED ->
                            throw IOException("Invalid neighbour state ${neighbour.stateValue}")
                    }
                    val lladdr = if (neighbour.hasLladdr()) {
                        neighbour.lladdr.toByteArray().also {
                            if (it.size != 6) throw IOException("Invalid neighbour link-layer address length ${it.size}")
                        }.let(MacAddress::fromBytes)
                    } else null
                    NeighbourDelta.Upsert(NetlinkNeighbour(neighbour.address.toInetAddress(), neighbour.`interface`,
                        lladdr, state))
                }
                DaemonProto.NeighbourDelta.DeltaCase.DELETE -> {
                    val delete = delta.delete
                    NeighbourDelta.Delete(delete.address.toInetAddress(), delete.`interface`)
                }
                DaemonProto.NeighbourDelta.DeltaCase.DELTA_NOT_SET -> throw IOException("Missing neighbour delta")
            }
        }
    }

    private fun command(description: String, build: DaemonProto.ClientEnvelope.Builder.() -> Unit) =
        Command(build, description)

    private fun SessionConfig.toProto() = DaemonProto.SessionConfig.newBuilder().also { proto ->
        proto.downstream = downstream
        proto.ipForward = ipForward
        proto.masquerade = when (masquerade) {
            Routing.MasqueradeMode.None -> DaemonProto.MasqueradeMode.MASQUERADE_MODE_NONE
            Routing.MasqueradeMode.Simple -> DaemonProto.MasqueradeMode.MASQUERADE_MODE_SIMPLE
            Routing.MasqueradeMode.Netd -> DaemonProto.MasqueradeMode.MASQUERADE_MODE_NETD
        }
        proto.ipv6Block = ipv6Block
        primaryNetwork?.let { proto.primaryNetwork = it.networkHandle }
        proto.addAllPrimaryRoutes(primaryRoutes.map { it.toIpv6PrefixProto() })
        fallbackNetwork?.let { proto.fallbackNetwork = it.networkHandle }
        proto.addAllPrimaryUpstreamInterfaces(primaryUpstreamInterfaces)
        proto.addAllFallbackUpstreamInterfaces(fallbackUpstreamInterfaces)
        proto.addAllClients(clients.map { client ->
            DaemonProto.ClientConfig.newBuilder()
                .setMac(client.mac.toByteArray().toByteString())
                .addAllIpv4(client.ipv4.map { it.toByteString() })
                .build()
        })
        ipv6Nat?.let {
            proto.ipv6Nat = DaemonProto.Ipv6NatConfig.newBuilder().setPrefixSeed(it.prefixSeed).build()
        }
    }.build()

    private fun IpPrefix.toIpv6PrefixProto(): DaemonProto.Ipv6Prefix {
        val address = address
        require(address is Inet6Address) { "IPv6 prefix expected: $this" }
        return DaemonProto.Ipv6Prefix.newBuilder()
            .setAddress(address.toByteString())
            .setPrefixLength(prefixLength)
            .build()
    }

    private fun InetAddress.toByteString() = address.toByteString()

    private fun ByteArray.toByteString() = ByteString.copyFrom(this)

    private fun ByteString.toInetAddress() = toByteArray().let { bytes ->
        when (bytes.size) {
            4, 16 -> InetAddress.getByAddress(bytes)
            else -> throw IOException("Invalid IP address length ${bytes.size}")
        }
    }
}
