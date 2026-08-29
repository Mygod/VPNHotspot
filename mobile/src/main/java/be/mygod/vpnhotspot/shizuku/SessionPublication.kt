package be.mygod.vpnhotspot.shizuku

import be.mygod.vpnhotspot.root.daemon.ShizukuSessionConfig

/**
 * Builds candidates against the last config actually written. A superseded candidate commits nothing, and
 * a raw network/index change advances the retirement generation unless [advanceUpstream] already did.
 */
class SessionPublication {
    /** Retires sockets pinned to an earlier upstream selection. */
    var upstreamGeneration = 1L
        private set

    private var published: ShizukuSessionConfig? = null

    fun advanceUpstream() = ++upstreamGeneration

    fun build(admit: Boolean, network: Long?, interfaceIndex: Int?) = published.let { published ->
        ShizukuSessionConfig(
            upstream_generation = if (published != null &&
                upstreamGeneration == published.upstream_generation &&
                (network != published.upstream_network || interfaceIndex != published.upstream_interface_index)) {
                upstreamGeneration + 1
            } else upstreamGeneration,
            admit = admit,
            upstream_network = network,
            upstream_interface_index = interfaceIndex,
        )
    }

    fun publish(config: ShizukuSessionConfig) = config.also {
        if (it.upstream_generation > upstreamGeneration) {
            upstreamGeneration = it.upstream_generation
        }
        published = it
    }
}
