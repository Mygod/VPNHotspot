package be.mygod.vpnhotspot.shizuku

import be.mygod.vpnhotspot.root.daemon.ShizukuSessionConfig
import okio.ByteString

/**
 * The rule the upstream axis follows.
 *
 * A field the daemon has pinned state behind may only move together with the axis that retires that state, so
 * a field that moved while its axis has not moved since the last publication has to advance it. An axis that
 * *has* already moved - because an observation advanced it out of band - has nothing left to do, and that is
 * what makes one logical change advance an axis exactly once no matter which half of it was noticed first.
 *
 * [published] is the axis value the last written config carried, so before anything has been published it is
 * zero and no counter can equal it: the first config never advances anything.
 */
internal fun retiring(counter: Long, published: Long, moved: Boolean) =
    if (moved && counter == published) counter + 1 else counter

/**
 * The three axes every [ShizukuSessionConfig] carries, and the one place a config is built from them.
 *
 * The daemon acknowledges all three together, and each answers a different question: the sequence says
 * *which* config was applied, [upstreamGeneration] says which `Network` the upstream sockets behind it were
 * bound to, and [downstreamEpoch] says whether a TUN-visible tuple still names the same client. A config
 * whose axes disagree with what it carries is refused outright - so the axes and the fields they retire have
 * to move together, which is why they are one owner rather than three counters kept beside the things that
 * advance them.
 *
 * # Building is not publishing
 *
 * [build] answers a *candidate*, and mutates nothing. The app coalesces to a single pending slot, so a config
 * that was built can be replaced before it is ever written, and every rule here is measured against the last
 * config that really went out - which [stamping] is what records. Committing at build time would let a
 * superseded candidate move the state its successor is compared against, and one logical change would then
 * advance an axis twice.
 *
 * The two address lists and [mtu] are constructor state for the same reason the axes are here. The addresses
 * come from the agent's `LinkProperties`, which a session builds once and never mutates, and the MTU is the
 * TestNetwork's own fixed contract - the value the TUN is created and the agent published with. The daemon
 * refuses a config that changes any of them without the axis that retires what is pinned behind it, and none
 * of them has one: holding them here is what makes that unrepresentable rather than merely true.
 *
 * Here rather than inside the session because a session cannot be constructed without the framework, and
 * this is the part with no framework in it: everything below is arithmetic over what was last published.
 */
class SessionPublication(
    private val virtualAddresses: List<ByteString>,
    private val gatewayAddresses: List<ByteString>,
    private val mtu: Int,
) {
    /**
     * Advanced when a config is actually written rather than when one is built, because the app coalesces to
     * a single pending slot and a superseded config is never sent at all. Assigning it at build time would
     * work too - the contract only asks for strictly increasing - but it would burn a number on a config
     * nothing ever acknowledged, and the acknowledgement is what this exists to match.
     */
    var sequence = 0L
        private set

    /**
     * Which `Network` upstream sockets are bound to. Starts at one so the daemon's zero-initialised view
     * always sees the first config as a change, and advances on every change to the selection - including a
     * `LinkProperties` change on the same `Network`, because a handle survives one that can invalidate the
     * state pinned behind it. It is deliberately not the handle itself: a handle is derived from a netId,
     * which is eventually reused by an unrelated network.
     */
    var upstreamGeneration = 1L
        private set

    /**
     * What retires everything the daemon keys to a downstream: its TUN-visible tuples, its queued output, and
     * the sizes that output was built against.
     *
     * One mover, because this mode is global rather than per-downstream: it publishes an upstream and never
     * looks at which interfaces Android is tethering behind it. What moves this is the session losing
     * positive confirmation that tethering is still carrying the exact network it owns - see
     * [advanceDownstream]. Starts at one so the daemon's zero-initialised view always sees the first config
     * as a change, and so the first published value is nonzero, which the daemon requires since zero is what
     * an unset proto field decodes to.
     */
    var downstreamEpoch = 1L
        private set

    /**
     * The generation, handle and interface index the last written config carried.
     *
     * The raw pair is tracked as well as the selection that produced it, because the two do not change
     * together: the session advances [upstreamGeneration] when the selected `Upstream` value changes, but the
     * interface index is resolved from its name while each config is *built*, so an index that was
     * unresolvable at selection time arrives on some later push with no observation behind it. The daemon
     * refuses that - a raw field that moved with nothing retiring the sockets bound behind it - so the pair is
     * compared here and the generation advances for it, coalesced with an explicit [advanceUpstream] through
     * [retiring] so one logical change never advances twice.
     */
    private var publishedGeneration = 0L
    private var publishedNetwork: Long? = null
    private var publishedInterfaceIndex: Int? = null

    /**
     * A loss of positive confirmation that tethering is still carrying this session's own network. Always
     * advances: the caller is saying the correspondence between a TUN-visible tuple and a client may already
     * be broken, because tethering can rebuild its NAT behind an unchanged `Network` handle. Answers the
     * epoch the next config will carry.
     */
    fun advanceDownstream() = ++downstreamEpoch

    /**
     * A change to the selected `Upstream`, which is a value rather than a handle: this is deliberately called
     * for a `LinkProperties` change that leaves the raw pair equal, because the state pinned behind that
     * `Network` is stale just the same. Answers the generation the next config will carry.
     */
    fun advanceUpstream() = ++upstreamGeneration

    /**
     * Builds the candidate for the observation that just happened, advancing each axis that the fields it
     * carries would otherwise move unretired. Commits nothing: see the class note.
     *
     * The sequence is left unset here for the same reason - this is a config that may yet be superseded
     * before it reaches the socket, and [stamping] is what runs when one really goes out.
     */
    fun build(admit: Boolean, network: Long?, interfaceIndex: Int?) = ShizukuSessionConfig(
        upstream_generation = retiring(upstreamGeneration, publishedGeneration,
            network != publishedNetwork || interfaceIndex != publishedInterfaceIndex),
        downstream_epoch = downstreamEpoch,
        admit = admit,
        upstream_network = network,
        upstream_interface_index = interfaceIndex,
        virtual_addresses = virtualAddresses,
        gateway_addresses = gatewayAddresses,
        downstream_mtu_floor = mtu,
    )

    /**
     * Stamps the config that is about to be written with the sequence its acknowledgement will name, and
     * records it as the publication every later candidate is measured against. Nonzero from the first one,
     * because zero is what an unset proto field decodes to and the daemon refuses it.
     *
     * [upstreamGeneration] is raised rather than assigned, because an observation may have advanced it
     * between this config being built and being written, and the counter may never walk backwards.
     */
    fun stamping(config: ShizukuSessionConfig) = config.copy(sequence = ++sequence).also { published ->
        if (published.upstream_generation > upstreamGeneration) {
            upstreamGeneration = published.upstream_generation
        }
        publishedGeneration = published.upstream_generation
        publishedNetwork = published.upstream_network
        publishedInterfaceIndex = published.upstream_interface_index
    }
}
