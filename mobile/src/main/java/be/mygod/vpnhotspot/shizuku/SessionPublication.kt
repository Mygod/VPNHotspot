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
 * The retirement counter every [ShizukuSessionConfig] carries, and the one place a config is built from it.
 *
 * The config call's ID already says which config the daemon acknowledged. [upstreamGeneration] answers the
 * different question: which `Network` the upstream sockets behind it were bound to. The generation is the
 * only retirement stamp there is - nothing the app can observe tells it that a TUN-visible tuple changed
 * hands, because Android's own conntrack owns that mapping - so admission opens and closes without retiring
 * anything. A config whose generation disagrees with the upstream fields it carries is refused outright,
 * which is why the counter and the fields it retires are one owner rather than a counter kept beside the
 * thing that advances it.
 *
 * # Building is not publishing
 *
 * [build] answers a *candidate*, and mutates nothing. The app coalesces to a single pending slot, so a config
 * that was built can be replaced before it is ever written, and every rule here is measured against the last
 * config that really went out - which [publish] is what records. Committing at build time would let a
 * superseded candidate move the state its successor is compared against, and one logical change would then
 * advance the generation twice.
 *
 * The two address lists are constructor state for the same reason the generation is here: they come from the
 * agent's `LinkProperties`, which a session builds once and never mutates, and the daemon refuses a config
 * that changes either of them at all, because nothing retires what is pinned behind them. Holding them here
 * is what makes that unrepresentable rather than merely true.
 *
 * Here rather than inside the session because a session cannot be constructed without the framework, and
 * this is the part with no framework in it: everything below is arithmetic over what was last published.
 */
class SessionPublication(
    private val virtualAddresses: List<ByteString>,
    private val gatewayAddresses: List<ByteString>,
) {
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
     * A change to the selected `Upstream`, which is a value rather than a handle: this is deliberately called
     * for a `LinkProperties` change that leaves the raw pair equal, because the state pinned behind that
     * `Network` is stale just the same. Answers the generation the next config will carry.
     */
    fun advanceUpstream() = ++upstreamGeneration

    /**
     * Builds the candidate for the observation that just happened, advancing the generation if the fields it
     * carries would otherwise move unretired. Commits nothing: see the class note.
     *
     * This candidate may yet be superseded before it reaches the socket, so [publish] is what records it as
     * the state later candidates are measured against.
     */
    fun build(admit: Boolean, network: Long?, interfaceIndex: Int?) = ShizukuSessionConfig(
        upstream_generation = retiring(upstreamGeneration, publishedGeneration,
            network != publishedNetwork || interfaceIndex != publishedInterfaceIndex),
        admit = admit,
        upstream_network = network,
        upstream_interface_index = interfaceIndex,
        virtual_addresses = virtualAddresses,
        gateway_addresses = gatewayAddresses,
    )

    /**
     * Records the config that is about to be written as the publication every later candidate is measured
     * against.
     *
     * [upstreamGeneration] is raised rather than assigned, because an observation may have advanced it
     * between this config being built and being written, and the counter may never walk backwards.
     */
    fun publish(config: ShizukuSessionConfig) = config.also { published ->
        if (published.upstream_generation > upstreamGeneration) {
            upstreamGeneration = published.upstream_generation
        }
        publishedGeneration = published.upstream_generation
        publishedNetwork = published.upstream_network
        publishedInterfaceIndex = published.upstream_interface_index
    }
}
