//! What one config says about where this session's traffic leaves, decoded once.
//!
//! Two facts, not one, and the difference is what this module exists for. The *selected network* is a handle
//! this app UID may bind a socket to; a resolver submission and a terminated TCP connection need nothing else.
//! The *relay upstream* is that same handle together with the interface index replies must arrive on, and it
//! is what a UDP or Echo mapping needs, because those match an unconnected reply against the mapping that sent
//! it and the interface check is what makes a reissued local port safe.
//!
//! Reconstructing either of them at each use is how the two drift apart. A `zip` of the two raw fields is a
//! third rule about what "no upstream" means, written wherever it happens to be needed, and one of those
//! copies eventually admits a shape the others refuse.
//!
//! # Zero is not a network
//!
//! `upstream_network` and `upstream_interface_index` are optional proto fields, so "absent" and "zero" are
//! different messages - but zero is what a default-constructed or truncated one decodes to, and it is also
//! what the platform reads as *the process's own default network*. Passing it to `android_setsocknetwork`
//! would silently bind upstream sockets to whatever the app UID's default happens to be, which is exactly the
//! fallback this mode does not have. So a present zero is refused here, before anything is published, rather
//! than being allowed to mean something further down.

use crate::shared::model::Network;
use crate::shared::proto::daemon::ShizukuSessionConfig;

/// The selected network, and the interface a reply to it must arrive on.
///
/// Both or neither: the interface check is what makes a reissued local port safe, so an upstream missing
/// either half is one the relays decline to serve rather than one they serve unchecked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayUpstream {
    pub network: Network,
    /// Nonzero by construction. Zero is not an interface index, and a mapping that accepted one would be
    /// matching replies against nothing.
    pub interface: u32,
}

/// Where this session's traffic leaves, as each owner needs it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Egress {
    /// What may be bound to. The resolver and the terminating TCP engine need this and nothing more: both
    /// connect, so the kernel picks the source and the reply arrives on the connection rather than on an
    /// interface that has to be checked.
    pub selected_network: Option<Network>,
    /// What an unconnected relay needs. Absent whenever the interface index is, which is not a failure - the
    /// session still resolves and still carries TCP, and the next config may complete it.
    pub relay_upstream: Option<RelayUpstream>,
}

/// Why a config's egress fields cannot be decoded. Terminal, like every other config refusal: a peer that
/// sent one is not one whose next message can be trusted either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// A present `upstream_network` of zero. See the module note: zero is the process's own default network,
    /// which is the one thing this mode may not fall back to.
    ZeroNetwork,
    /// A present `upstream_interface_index` of zero.
    ZeroInterface,
    /// An interface index with no network to go with it. There is nothing to bind, so the index names the
    /// arrival side of a path that has no departure side.
    InterfaceWithoutNetwork,
}

/// Decodes one config's egress fields, or refuses the config.
///
/// The three shapes that are accepted are the three a real session goes through: nothing selected yet,
/// selected but not yet told which interface replies arrive on, and both. Everything else is a message whose
/// sender and this daemon disagree about what the fields mean.
pub fn decode(config: &ShizukuSessionConfig) -> Result<Egress, Rejected> {
    let network = match config.upstream_network {
        Some(0) => return Err(Rejected::ZeroNetwork),
        other => other,
    };
    let interface = match config.upstream_interface_index {
        Some(0) => return Err(Rejected::ZeroInterface),
        other => other,
    };
    match (network, interface) {
        (None, Some(_)) => Err(Rejected::InterfaceWithoutNetwork),
        (None, None) => Ok(Egress::default()),
        (Some(network), None) => Ok(Egress {
            selected_network: Some(network),
            relay_upstream: None,
        }),
        (Some(network), Some(interface)) => Ok(Egress {
            selected_network: Some(network),
            relay_upstream: Some(RelayUpstream { network, interface }),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(network: Option<u64>, interface: Option<u32>) -> ShizukuSessionConfig {
        ShizukuSessionConfig {
            upstream_generation: 1,
            admit: true,
            upstream_network: network,
            upstream_interface_index: interface,
            virtual_addresses: Vec::new(),
            gateway_addresses: Vec::new(),
        }
    }

    /// The three shapes a real session goes through, and what each owner gets from them.
    #[test]
    fn the_three_accepted_shapes_answer_each_owner_separately() {
        assert_eq!(decode(&config(None, None)), Ok(Egress::default()));

        // Selected but not yet told which interface: the resolver and TCP may work, the relays may not.
        assert_eq!(
            decode(&config(Some(0x1234), None)),
            Ok(Egress {
                selected_network: Some(0x1234),
                relay_upstream: None,
            })
        );

        // Both. The relay upstream carries the *same* handle, which is what stops the two from drifting.
        let whole = decode(&config(Some(0x1234), Some(7))).expect("accepted");
        assert_eq!(whole.selected_network, Some(0x1234));
        assert_eq!(
            whole.relay_upstream,
            Some(RelayUpstream {
                network: 0x1234,
                interface: 7,
            })
        );
        assert_eq!(
            whole.relay_upstream.map(|relay| relay.network),
            whole.selected_network,
            "one network, named twice"
        );
    }

    /// Zero never reaches a socket, whatever it is paired with.
    #[test]
    fn a_present_zero_is_refused_rather_than_bound() {
        assert_eq!(decode(&config(Some(0), None)), Err(Rejected::ZeroNetwork));
        assert_eq!(
            decode(&config(Some(0), Some(7))),
            Err(Rejected::ZeroNetwork)
        );
        assert_eq!(
            decode(&config(Some(0), Some(0))),
            Err(Rejected::ZeroNetwork)
        );
        assert_eq!(
            decode(&config(Some(0x1234), Some(0))),
            Err(Rejected::ZeroInterface)
        );
        assert_eq!(
            decode(&config(None, Some(0))),
            Err(Rejected::ZeroInterface),
            "checked before the pairing, because zero is wrong on its own"
        );
    }

    /// An arrival interface with nothing to depart on is a config the two sides disagree about.
    #[test]
    fn an_interface_without_a_network_is_refused() {
        assert_eq!(
            decode(&config(None, Some(7))),
            Err(Rejected::InterfaceWithoutNetwork)
        );
    }
}
