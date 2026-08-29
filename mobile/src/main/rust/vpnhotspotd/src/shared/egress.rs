use crate::shared::model::Network;
use crate::shared::proto::daemon::ShizukuSessionConfig;

/// The selected network, and the interface a reply to it must arrive on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayUpstream {
    pub network: Network,
    /// Nonzero by construction. Zero is not an interface index, and a mapping that accepted one would be
    /// matching replies against nothing.
    pub interface: u32,
}

/// Selected network and the optional interface required by unconnected relays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Egress {
    pub network: Option<Network>,
    pub interface: Option<u32>,
}

impl Egress {
    pub fn relay_upstream(self) -> Option<RelayUpstream> {
        Some(RelayUpstream {
            network: self.network?,
            interface: self.interface?,
        })
    }
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
            network: Some(network),
            interface: None,
        }),
        (Some(network), Some(interface)) => Ok(Egress {
            network: Some(network),
            interface: Some(interface),
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
        }
    }

    #[test]
    fn the_three_accepted_shapes_answer_each_owner_separately() {
        assert_eq!(decode(&config(None, None)), Ok(Egress::default()));

        assert_eq!(
            decode(&config(Some(0x1234), None)),
            Ok(Egress {
                network: Some(0x1234),
                interface: None,
            })
        );

        let whole = decode(&config(Some(0x1234), Some(7))).expect("accepted");
        assert_eq!(whole.network, Some(0x1234));
        assert_eq!(
            whole.relay_upstream(),
            Some(RelayUpstream {
                network: 0x1234,
                interface: 7,
            })
        );
    }

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
        assert_eq!(decode(&config(None, Some(0))), Err(Rejected::ZeroInterface));
    }

    #[test]
    fn an_interface_without_a_network_is_refused() {
        assert_eq!(
            decode(&config(None, Some(7))),
            Err(Rejected::InterfaceWithoutNetwork)
        );
    }
}
